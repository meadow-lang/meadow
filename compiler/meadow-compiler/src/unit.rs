//! Per-compilation-unit driver: **resolve → infer → lower** a set of
//! already-parsed modules, against a set of already-compiled dependency
//! packages, into one typed and lowered [`CompiledPackage`].
//!
//! This is the compiler's headline entry point. Everything *around* it —
//! filesystem package discovery, the package graph, linking, and the embedded
//! standard library — lives in the `meadow` build crate.

use crate::{
    Options, ast, core,
    diagnostics::{Diagnostic, from_parse_error},
    exhaust,
    hir::{self, VarId},
    infer::{Infer, InferResult, Scheme, TypeTable},
    intern::InternedString,
    lexer::tokenize,
    parser,
    rename::{self, NameRef, Resolver},
    scc,
    source::{Source, SourceKind},
};
use std::collections::HashMap;

/// One parsed module handed to [`compile_unit`].
pub struct AstModule {
    /// Dotted path from the package's source root; empty for the root module.
    pub path: Vec<InternedString>,
    pub name: InternedString,
    pub ast: ast::LModule,
    /// Where the text came from.
    ///
    /// A [`Span`](meadow_span::Span) is a pair of offsets and nothing else — it
    /// carries no record of which file it indexes. That is fine while a
    /// consumer only ever looks at one module, and not fine the moment one has
    /// to *name* a position: an editor answering go-to-definition across
    /// modules reads the file from here.
    pub source: Source,
}

/// A resolved module, paired with its position in the package.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TypedModule {
    pub path: Vec<InternedString>,
    pub name: InternedString,
    pub hir: hir::LModule,
    /// Carried through from [`AstModule::source`] — what this module's spans are
    /// offsets into.
    pub source: Source,
    /// Names written in this module that the HIR does not keep: the contents
    /// of its `use` lists, and the type qualifying a `Type.Ctor`. An editor
    /// renaming a name has to rewrite these too.
    pub refs: Vec<rename::RefSite>,
}

/// An exported top-level binding: its name, its `VarId`, its inferred scheme, and
/// the dotted path of the module it was declared in (empty for a single-module
/// package). A dependent reaches it as `<pkg>.<module path>.<name>` via `use`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Export {
    pub name: InternedString,
    pub var: VarId,
    pub scheme: Scheme,
    pub module: Vec<InternedString>,
    /// Whether it was written `@macro`, and so may be called as one by a
    /// package that imports it with a `!`. A macro is an ordinary function and
    /// nothing about its type makes it one: saying so is what does.
    pub is_macro: bool,
}

/// The output of [`compile_unit`]: one package, fully typed and lowered.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CompiledPackage {
    /// Caller-assigned id — the build system uses the package-graph index, the
    /// REPL uses the line number. Not interpreted here.
    pub id: usize,
    /// Constructors a dependent may write **unqualified**, by canonical name.
    ///
    /// A constructor's real name is `Type.Ctor`, so by default a dependent has
    /// to say which type it meant. This is the escape hatch the prelude uses:
    /// `@pub use Std.Maybe.Maybe.*` lists `Maybe.Just` and `Maybe.None` here,
    /// which is why `Just` needs no qualifier anywhere.
    pub flat_ctors: Vec<InternedString>,
    /// Types, effects and traits a dependent may name **unqualified**, by
    /// canonical name: what a `@pub use` names. A dependency's are in scope
    /// only where a module asks for them, and the prelude's -- the standard
    /// library's `@pub use Std.Maybe (Maybe)` -- are asked for everywhere.
    #[serde(default)]
    pub flat_types: Vec<InternedString>,
    /// The `VarId` range this unit minted: `start..end`.
    ///
    /// A unit starts above everything its dependencies used, so the ranges of
    /// a package and everything under it never overlap. Recorded rather than
    /// recomputed because it is the provenance of an id — which unit owns it —
    /// and because a unit stacked on this one needs `end` to start from.
    pub vars: std::ops::Range<u32>,
    pub name: InternedString,
    /// Who this package *is*, for naming the types and effects it declares:
    /// `name@version`, or just the name when there is no version to add.
    ///
    /// Two copies of one package at different versions are different packages,
    /// so their types are different types. The name alone cannot say that --
    /// both call themselves `json` -- and conflating them would let a value of
    /// one pass for the other. Messages never show this: `hir::spelling` takes
    /// the name off the front.
    pub ident: InternedString,
    /// The macros this package lets others see, as the token trees they were
    /// written with. A dependent expands one without re-parsing this package's
    /// source, which is the whole reason they are stored rather than the
    /// matchers they read as.
    pub macros: Vec<crate::expand::Rules>,
    /// The compile-time bindings this package's macros defined and its
    /// `@compileTime def`s wrote: what a macro of a dependent can `lookup`,
    /// as far as each one's visibility says.
    pub bindings: Vec<crate::expand::Binding>,
    /// Files this package embedded with `includeStr`, and what each hashed to.
    ///
    /// Inputs to the build that its own sources do not mention: without them a
    /// cached build would be reused after an embedded file had changed.
    pub embedded: Vec<(String, u64)>,
    /// How the operators this package or its dependencies declared bind:
    /// `infixr 5 ++`. Global by spelling, so every dependent takes them all.
    pub fixities: Vec<(InternedString, hir::Fixity)>,
    /// This package's `main`, if its root module declares one.
    ///
    /// An entry point is not an export: nothing links against `main`, the
    /// runtime calls it. Finding it here rather than among the exports is what
    /// lets a program keep its declarations to itself — before this, adding
    /// a visibility attribute anywhere in a package meant `main` needed one too or the linker
    /// would report no entry point at all.
    ///
    /// It is `() -> () ! Eff`, and a program runs by calling it.
    pub entry: Option<VarId>,
    /// What runs instead of `main` when the build asked for another entry by
    /// name ([`crate::Options::entry_name`]): a value, evaluated and shown --
    /// the REPL's `it`, a debugger's expression, a test's `result`.
    pub value_entry: Option<VarId>,
    pub modules: Vec<TypedModule>,
    /// Whole-package node -> type table (node ids are dense across the package).
    pub types: TypeTable,
    pub exports: Vec<Export>,
    /// The scheme of every binding this unit generalized: its top-level
    /// bindings, exported or not, and its local `let`s and `let rec`s.
    ///
    /// The type table records what each *node* was, which for a local binding
    /// is a type with its variables still free -- and a free effect variable
    /// prints as `! c`, even when nothing constrains it. The scheme is what
    /// the binding generalized to, and a quantifier that appears once is
    /// shown for what it is: nothing. An editor showing a local binding the way
    /// it shows a top-level one needs this.
    pub generalized: HashMap<VarId, Scheme>,
    pub defs: Vec<core::Def>,
    /// Named-field order per constructor declared in this package.
    pub ctor_fields: HashMap<InternedString, Vec<InternedString>>,
    /// Constructors' field types, by type name: this package's and its
    /// dependencies' -- see `core::Program::variants`.
    pub variants: meadow_infer::VariantEnv,
    /// This package's resolved `data` / `record` / `effect` declarations,
    /// re-imported by dependents (and by later REPL lines).
    pub data_decls: Vec<hir::LDecl>,
    /// `@test` functions declared in this unit: `(name, its VarId)`, in
    /// declaration order. `meadow test` calls each with `()`.
    pub tests: Vec<(InternedString, VarId)>,
    /// Names a dependent gets **unqualified** automatically (the package's prelude
    /// re-exports). `None` = flat-import everything (REPL prefixes, ad-hoc `deps`);
    /// `Some(list)` = only these are flat, the rest need `use`.
    pub prelude_exports: Option<Vec<InternedString>>,
    /// Whether this is a package a manifest depends on: its values are named
    /// with `use` -- `use Util`, `use Util (double)` -- and none is in a
    /// dependent's scope otherwise, whatever `prelude_exports` says of its
    /// constructors. A REPL line and an ad-hoc dependency are not packages.
    #[serde(default)]
    pub package: bool,
    /// This package's functions that copy one of their parameters into a
    /// compact region, with which: checked at a dependent's calls as
    /// `compact` is. See `meadow_exhaust::compacting_wrappers`.
    #[serde(default)]
    pub compacting: Vec<(VarId, usize)>,
}

/// A `@test` function, and the module it is declared in.
#[derive(Debug, Clone)]
pub struct TestSite {
    /// The dotted path of the declaring module; empty for the package root.
    pub module: Vec<InternedString>,
    pub name: InternedString,
    pub var: VarId,
}

impl TestSite {
    /// How a test is named to someone choosing one: `Parser.handlesEmpty`, or
    /// just `handlesEmpty` in the root module.
    ///
    /// The module is part of the name because modules are namespaces: two of
    /// them may each declare a test called `works`, and a runner asked for one
    /// by its bare name cannot tell which was meant. `meadow test` prints this
    /// and matches against it, and the editor's Test lens asks for it, so the
    /// spelling lives here and nowhere else.
    pub fn qualified(&self) -> String {
        if self.module.is_empty() {
            return self.name.to_string();
        }
        let mut out = String::new();
        for seg in &self.module {
            out.push_str(seg);
            out.push('.');
        }
        out.push_str(&self.name);
        out
    }
}

/// A dependency as the package being compiled knows it.
///
/// `spelled` is what this package writes in a `use` -- the name in its
/// `[dependencies]`, which `meadow add --rename` may have changed. The package
/// itself may call itself something else, and two dependencies may call
/// themselves the same thing, so what a `use` names is asked of this rather
/// than of the package.
#[derive(Clone, Copy)]
pub struct Dep<'a> {
    pub spelled: InternedString,
    pub pkg: &'a CompiledPackage,
}

impl<'a> Dep<'a> {
    /// A dependency known by the name it calls itself.
    pub fn new(pkg: &'a CompiledPackage) -> Dep<'a> {
        Dep {
            spelled: pkg.name,
            pkg,
        }
    }

    /// The same, under a name the dependent chose.
    pub fn named(spelled: InternedString, pkg: &'a CompiledPackage) -> Dep<'a> {
        Dep { spelled, pkg }
    }
}

impl<'a> std::ops::Deref for Dep<'a> {
    type Target = CompiledPackage;

    fn deref(&self) -> &CompiledPackage {
        self.pkg
    }
}

impl CompiledPackage {
    /// Every `@test`, with its module, in declaration order.
    ///
    /// [`CompiledPackage::tests`] records only name and id, so the module is
    /// recovered from which module's top-level bindings introduce the id.
    pub fn test_sites(&self) -> Vec<TestSite> {
        let mut module_of: HashMap<VarId, &[InternedString]> = HashMap::new();
        for m in &self.modules {
            for d in &m.hir.value().decls {
                if let hir::Decl::Bind(b) = d.value() {
                    for v in b.bound_vars() {
                        module_of.insert(v, &m.path);
                    }
                }
            }
        }
        self.tests
            .iter()
            .map(|&(name, var)| TestSite {
                module: module_of.get(&var).map(|p| p.to_vec()).unwrap_or_default(),
                name,
                var,
            })
            .collect()
    }
}

/// Compile a single source string as a one-module, dependency-free package.
///
/// Convenience for tests and quick experiments — lex → parse → resolve → infer →
/// lower, with every diagnostic collected (never fatal). Use the `meadow` build
/// crate's `compile_str_with_std` / `build` when the standard library or a
/// package graph is needed.
pub fn compile_str(name: &str, src: &str) -> (CompiledPackage, Vec<Diagnostic>) {
    compile_str_with(name, src, Options::default())
}

/// [`compile_str`] under an explicit set of compiler [`Options`].
pub fn compile_str_with(
    name: &str,
    src: &str,
    opts: Options,
) -> (CompiledPackage, Vec<Diagnostic>) {
    let name = InternedString::from(name);
    let source = Source::new(SourceKind::Interactive, InternedString::from(src));
    let lex = tokenize(source);
    let mut diags: Vec<Diagnostic> = lex.errors;
    let (ast, perrs) = parser::parse(name, source, &lex.tokens);
    for e in &perrs {
        diags.push(from_parse_error(&source.name().to_string(), e));
    }
    let modules = ast
        .map(|ast| {
            vec![AstModule {
                path: vec![],
                name,
                ast,
                source,
            }]
        })
        .unwrap_or_default();
    let (cp, unit_diags) = compile_unit(name, 0, modules, &[], opts);
    diags.extend(unit_diags);
    (cp, diags)
}

/// Resolve → infer → lower a set of already-parsed modules. Shared by the batch
/// build and the REPL. Modules within a unit may be mutually recursive.
pub fn compile_unit(
    unit_name: InternedString,
    id: usize,
    modules: Vec<AstModule>,
    deps: &[Dep<'_>],
    opts: Options,
) -> (CompiledPackage, Vec<Diagnostic>) {
    compile_unit_in_package(unit_name, unit_name, id, modules, deps, opts)
}

/// Like [`compile_unit`], but `pkg` names the umbrella package (so a `use pkg.a.b`
/// from a sibling sub-module resolves against a peer named `a.b`). The embedded
/// stdlib compiles each of its modules as its own unit with `pkg = "Std"`.
pub fn compile_unit_in_package(
    pkg: InternedString,
    unit_name: InternedString,
    id: usize,
    modules: Vec<AstModule>,
    deps: &[Dep<'_>],
    opts: Options,
) -> (CompiledPackage, Vec<Diagnostic>) {
    compile_unit_above(pkg, unit_name, id, modules, deps, opts, 0)
}

/// [`compile_unit_above`], for a package that is one of several copies of
/// itself: `ident` is who it is (`name@version`), which is what its types are
/// named after, while `pkg` stays what its own modules call it.
#[allow(clippy::too_many_arguments)]
pub fn compile_unit_as(
    pkg: InternedString,
    ident: InternedString,
    unit_name: InternedString,
    id: usize,
    modules: Vec<AstModule>,
    deps: &[Dep<'_>],
    opts: Options,
    floor: u32,
) -> (CompiledPackage, Vec<Diagnostic>) {
    compile_unit_inner(pkg, ident, unit_name, id, modules, deps, opts, floor, None)
}

/// [`compile_unit_in_package`], minting no variable below `floor`.
///
/// Starting above the unit's own dependencies keeps it apart from them, but
/// not from a package beside it: two packages that both depend only on `util`
/// would start at the same id, and linking both into one program -- `app`
/// depending on each, or a workspace testing both -- has one overwrite the
/// other's definitions. A build compiling many packages passes the end of
/// everything it has compiled so far.
pub fn compile_unit_above(
    pkg: InternedString,
    unit_name: InternedString,
    id: usize,
    modules: Vec<AstModule>,
    deps: &[Dep<'_>],
    opts: Options,
    floor: u32,
) -> (CompiledPackage, Vec<Diagnostic>) {
    compile_unit_inner(pkg, pkg, unit_name, id, modules, deps, opts, floor, None)
}

/// [`compile_unit_as`], with something that can run a procedural macro.
///
/// Running one means linking a program and evaluating it, which is not
/// something this crate does -- so a build hands in a
/// [`Runner`](crate::expand::proc::Runner) and everything else compiles
/// without one. A unit that calls a procedural macro where there is no runner
/// is told so, rather than compiled as if the call were not there.
#[allow(clippy::too_many_arguments)]
pub fn compile_unit_with_procs(
    pkg: InternedString,
    ident: InternedString,
    unit_name: InternedString,
    id: usize,
    modules: Vec<AstModule>,
    deps: &[Dep<'_>],
    opts: Options,
    floor: u32,
    procs: &dyn crate::expand::proc::Runner,
) -> (CompiledPackage, Vec<Diagnostic>) {
    compile_unit_inner(
        pkg,
        ident,
        unit_name,
        id,
        modules,
        deps,
        opts,
        floor,
        Some(procs),
    )
}

#[allow(clippy::too_many_arguments)]
/// `MEADOW_TIME_PASSES=1`: how long each phase of compiling a unit took, on
/// stderr -- the front end's half of what `meadow_seq::lower_program` says of
/// the back end's.
struct Phase(Option<std::time::Instant>);

impl Phase {
    fn start() -> Phase {
        Phase(
            std::env::var_os("MEADOW_TIME_PASSES")
                .is_some()
                .then(std::time::Instant::now),
        )
    }

    fn done(&mut self, name: &str) {
        if let Some(at) = &mut self.0 {
            eprintln!("  {name:<14} {:>10.1?}", at.elapsed());
            *at = std::time::Instant::now();
        }
    }
}

fn compile_unit_inner(
    pkg: InternedString,
    ident: InternedString,
    unit_name: InternedString,
    id: usize,
    modules: Vec<AstModule>,
    deps: &[Dep<'_>],
    opts: Options,
    floor: u32,
    procs: Option<&dyn crate::expand::proc::Runner>,
) -> (CompiledPackage, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let filename = unit_name.to_string();
    let mut phase = Phase::start();

    // `@cfg(…)`: what does not apply to this build is gone before anything
    // else looks. Then macros, on what is left -- a call under a `@cfg` that
    // does not hold is never expanded, and one a macro produces is read here.
    let mut modules = modules;
    // `mod Foo { … }` is a module like one with a file, from here on.
    lift_inline_modules(&mut modules);
    // A child whose `mod` its parent declares only under a `@cfg` that does not
    // hold is not part of this build either, and nor is anything below it --
    // `@cfg(test) mod Tests` leaves `Tests.mw` out of an ordinary build.
    let mut dropped: Vec<Vec<InternedString>> = Vec::new();
    for m in &mut modules {
        let here = module_filename(&filename, m.source);
        let before = declared_mods(&m.ast.value);
        crate::cfg::strip(&mut m.ast.value, opts, &here, &mut diags);
        let after = declared_mods(&m.ast.value);
        for name in before.difference(&after) {
            let mut child = m.path.clone();
            child.push(*name);
            dropped.push(child);
        }
    }
    modules.retain(|m| !dropped.iter().any(|d| m.path.starts_with(d)));
    // Macros: read from every module of the unit, then expanded in each.
    let (macros, bindings, expansions) =
        crate::expand::expand_unit(pkg, &mut modules, deps, procs, &filename, &mut diags);

    phase.done("expand");
    // --- name resolution (whole unit at once, so modules may be mutually recursive)
    // Start above every dependency, so no two units can mint the same id and
    // an id can be traced back to the unit that owns it.
    let var_base = deps
        .iter()
        .map(|d| d.vars.end)
        .max()
        .unwrap_or(0)
        .max(floor);
    let mut resolver = Resolver::with_prelude(filename.clone(), var_base);
    resolver.set_package(ident);
    resolver.set_standalone(deps.is_empty());
    // Every dependency's types are *known* here: what a `use` can bring, and
    // what a canonical name means. A module names one only once it has asked
    // for it -- `use M (Ty)`, `use M` -- or the prelude re-exports it; a REPL
    // prefix or an ad-hoc dependency alone puts all of its in scope.
    for dep in deps {
        resolver.import_types(
            &dep.data_decls,
            dep.prelude_exports.is_none() && !dep.package,
        );
    }
    for dep in deps {
        match &dep.prelude_exports {
            // A package: a module asks for what it names.
            None if dep.package => {}
            // A REPL prefix or an ad-hoc dep: its values are flat, but its
            // constructors are still under their types -- `Colour.Red`, or bare
            // after `use Colour.*` -- except one named like its type.
            None => {
                for ty in resolver.imported_type_names() {
                    resolver.use_struct_ctor(ty);
                }
            }
            // The prelude: the types and constructors it re-exports.
            Some(_) => {
                for t in &dep.flat_types {
                    resolver.use_named_type(*t);
                }
                for c in &dep.flat_ctors {
                    resolver.use_flat_ctor(*c);
                }
            }
        }
    }
    // Flat value imports: a dep with `prelude_exports = None` (a REPL prefix, an
    // ad-hoc `compile_str` dep) contributes *everything*; one with `Some(list)`
    // contributes only `list` unqualified, the rest reachable via `use`.
    for dep in deps {
        match &dep.prelude_exports {
            // A package: what a module wants of it, it names with `use`.
            None if dep.package => {}
            None => {
                for e in &dep.exports {
                    resolver.import(e.name, e.var);
                }
            }
            // The standard library's prelude.
            Some(flat) => {
                for e in &dep.exports {
                    if e.module.is_empty() && flat.contains(&e.name) {
                        resolver.import_prelude(e.name, e.var);
                    }
                }
                // An effect it re-exports brings its operations, as naming
                // one in a `use` does.
                for d in &dep.data_decls {
                    if let hir::Decl::Effect(ed) = d.value()
                        && dep.flat_types.contains(&ed.name)
                    {
                        for (op, id, _) in &ed.ops {
                            resolver.import_prelude(*op, *id.value());
                        }
                    }
                }
            }
        }
    }
    // Everything above is shared by every module of the unit. What follows
    // belongs to one module at a time.
    resolver.seal_base_scope();

    for dep in deps {
        for (op, fixity) in &dep.fixities {
            resolver.import_fixity(*op, *fixity);
        }
    }
    for m in &modules {
        resolver.set_filename(module_filename(&filename, m.source));
        resolver.declare_fixities(&m.ast.value().decls);
    }

    // Declare first, all modules, so that a `use` can name a sibling and so
    // that mutual recursion across modules keeps working — the package is one
    // compilation unit, and only the *namespaces* are per module.
    for m in &modules {
        resolver.set_filename(module_filename(&filename, m.source));
        resolver.set_module(&m.path);
        resolver.declare_types(&m.ast.value().decls);
    }
    for m in &modules {
        resolver.set_filename(module_filename(&filename, m.source));
        resolver.set_module(&m.path);
        resolver.declare_toplevel(&m.ast.value().decls);
    }

    // Names written bare that mean more than one thing, across the whole unit
    // (node ids are unit-wide). Inference chooses; see `hir::Overloads`.
    // What `@pub use M.Ty.*` re-exports unqualified; see `flat_ctors` below.
    let mut flat_ctors: Vec<InternedString> = Vec::new();
    // And what `@pub use M (Ty)` re-exports; see `flat_types`.
    let mut flat_types: Vec<InternedString> = Vec::new();
    let mut overloads: hir::Overloads = HashMap::new();
    let mut typed: Vec<TypedModule> = modules
        .iter()
        .map(|m| {
            // Its own declarations and nothing else, then whatever it asked
            // for. A sibling's names are never free.
            let here = module_filename(&filename, m.source);
            resolver.set_filename(here.clone());
            resolver.enter_module(&m.path);
            // The modules under this one qualify by the path from here, as
            // Rust's do: after `mod Core`, `Core.name`, and `Core.Expr.name`
            // for a module `Expr` inside it. A `use … as Core` written here
            // comes after, and so wins.
            for below in &modules {
                if below.path.len() > m.path.len() && below.path.starts_with(&m.path) {
                    let qualifier = InternedString::from(dotted(&below.path[m.path.len()..]));
                    resolver.activate_child(qualifier, &below.path);
                }
            }
            for d in &m.ast.value().decls {
                let base = match d.value() {
                    ast::Decl::Attributed(_, inner) => inner.value(),
                    other => other,
                };
                if let ast::Decl::Use(u) = base {
                    let (brought, types) =
                        apply_use(&mut resolver, pkg, u, deps, &here, &mut diags);
                    // `@pub use M.Ty.*`: the package's *unqualified* surface,
                    // which is a claim about what a dependent may write bare.
                    // Plain `@pub` only -- an argument only ever narrows it.
                    if let ast::Decl::Attributed(attrs, _) = d.value() {
                        if attrs
                            .iter()
                            .any(|a| &**a.name.value() == "pub" && a.args.is_empty())
                        {
                            flat_ctors.extend(brought);
                            flat_types.extend(types);
                        }
                    }
                }
            }
            let mut hir = resolver.resolve_module(&m.ast);
            overloads.extend(resolver.take_overloads());
            // Reorder the top-level bindings by dependency and record their
            // groups, so inference (and evaluation) never meets a name before the
            // thing that defines it.
            scc::group_module(&mut hir.value, &overloads);
            TypedModule {
                path: m.path.clone(),
                name: m.name,
                hir,
                source: m.source,
                refs: resolver.take_extra_refs(),
            }
        })
        .collect();
    // Same again one level up: a unit's modules are discovered in alphabetical
    // order, which says nothing about what depends on what, so they need
    // sorting too.
    let order = scc::module_order(typed.iter().map(|m| m.hir.value()), &overloads);
    typed = permute(typed, &order);
    diags.extend(resolver.take_errors());

    // The entry point, from the root module alone: a `main` in a submodule is
    // an ordinary function that happens to be called `main`. Found before
    // inference, which holds `main` to a different rule than every other `def`.
    let root_named = |name: InternedString| {
        typed
            .iter()
            .filter(|m| m.path.is_empty())
            .flat_map(|m| m.hir.value().decls.iter().map(move |d| (m.source, d)))
            .filter_map(|(source, d)| match d.value() {
                hir::Decl::Bind(bind) => bind
                    .bound_vars()
                    .into_iter()
                    .find(|id| resolver.names().get(id) == Some(&name))
                    // Where its name is written, for what is said of it.
                    .map(|id| match bind {
                        hir::Bind::Fun(n, ..) => (id, n.span, source),
                        _ => (id, d.span, source),
                    }),
                _ => None,
            })
            .next()
    };
    let main_at = root_named(InternedString::from("main"));
    let entry = main_at.map(|(id, _, _)| id);
    // And whatever the caller runs in its place, in any module: a value, which
    // is evaluated and shown rather than called.
    let mut runs: Vec<VarId> = entry.into_iter().collect();
    let mut value_entry = None;
    if let Some(name) = opts.entry_name {
        let name = InternedString::from(name);
        value_entry = root_named(name).map(|(id, _, _)| id);
        runs.extend(
            resolver
                .names()
                .iter()
                .filter(|(_, n)| **n == name)
                .map(|(id, _)| *id),
        );
    }

    phase.done("resolve");
    // --- type inference (one arena for the whole unit + dependency schemes)
    let mut infer = Infer::new(filename.clone(), resolver.id_count());
    infer.set_entries(runs);
    infer.set_overloads(overloads.clone());
    infer.load_prelude(&resolver.prelude_bindings());
    let dep_schemes: Vec<(VarId, Scheme)> = deps
        .iter()
        .flat_map(|d| d.exports.iter().map(|e| (e.var, e.scheme.clone())))
        .collect();
    infer.load_deps(&dep_schemes);
    for dep in deps {
        infer.register_types(&dep.data_decls);
    }
    for m in &typed {
        infer.set_filename(module_filename(&filename, m.source));
        infer.register_types(&m.hir.value().decls);
        // This unit's own effect operations are part of its export surface, so a
        // dependent can write `State.get` rather than relying on them being
        // injected into every scope (where an ordinary value can shadow them).
        infer.export_effect_ops(&m.hir.value().decls);
    }
    // `impl`s after every trait is known, a sibling module's included.
    for dep in deps {
        infer.register_impls(&dep.data_decls);
    }
    for m in &typed {
        infer.set_filename(module_filename(&filename, m.source));
        infer.register_impls(&m.hir.value().decls);
        infer.export_traits(&m.hir.value().decls);
    }
    // The bindings of every module, as one graph: a module whose tests call a
    // sibling that uses it must not see the sibling's names before they have
    // types. See `meadow_scc::unit_groups`.
    {
        let hirs: Vec<&hir::LModule> = typed.iter().map(|m| &m.hir).collect();
        let files: Vec<String> = typed
            .iter()
            .map(|m| module_filename(&filename, m.source))
            .collect();
        let groups: Vec<(Vec<(usize, usize)>, bool)> = scc::unit_groups(
            &hirs.iter().map(|m| m.value()).collect::<Vec<_>>(),
            &overloads,
        )
        .into_iter()
        .map(|g| (g.members, g.recursive))
        .collect();
        infer.infer_unit(&hirs, &files, &groups);
    }
    // The bodies inside traits and `impl`s last: they may mention any binding
    // of the unit, and no binding needs more of them than their types.
    for m in &typed {
        infer.set_filename(module_filename(&filename, m.source));
        infer.infer_traits(&m.hir);
    }
    if let Some((main, at, source)) = main_at {
        infer.set_filename(module_filename(&filename, source));
        infer.check_main(main, at);
    }
    let InferResult {
        table,
        schemes,
        generalized,
        variants,
        resolutions,
        evidence,
        traits,
        errors,
    } = infer.finish();
    diags.extend(errors);
    // From here on an overloaded name is whichever candidate inference chose,
    // so nothing downstream has to know it was ever in question.
    for m in &mut typed {
        hir::apply_resolutions(&mut m.hir, &resolutions);
    }

    phase.done("infer");
    // --- pattern coverage (needs the types; runs before lowering discards them)
    // -- and what may go in a compact, which needs them too.
    let mut compacting: HashMap<VarId, usize> = resolver
        .prelude_bindings()
        .into_iter()
        .filter_map(|(name, id)| match &*name {
            "compact" => Some((id, 0)),
            "compactAdd" => Some((id, 1)),
            _ => None,
        })
        .collect();
    for dep in deps {
        compacting.extend(dep.compacting.iter().copied());
    }
    let hirs: Vec<&hir::LModule> = typed.iter().map(|m| &m.hir).collect();
    let own_compacting = exhaust::compacting_wrappers(&hirs, &mut compacting);
    let synonyms = exhaust::synonyms(&hirs, resolver.names());
    for m in &typed {
        diags.extend(exhaust::check_module(
            &module_filename(&filename, m.source),
            &m.hir,
            &table,
            &variants,
            opts.check_exhaustive(),
            &compacting,
            &synonyms,
        ));
    }

    phase.done("coverage");
    // --- lower to core
    let prims = prim_map(&resolver);
    let names = resolver.names().clone();
    let effect_ops: HashMap<VarId, (InternedString, InternedString)> = resolver
        .effect_op_vars()
        .into_iter()
        .map(|(id, eff, op)| (id, (eff, op)))
        .collect();
    let ctor_arity = resolver.ctor_arities();
    // Every polymorphic name a mention could refer to: this unit's bindings,
    // and the ones its dependencies exported. Lowering needs them to give each
    // mention its type arguments.
    let mut all_schemes: HashMap<VarId, Scheme> = dep_schemes.iter().cloned().collect();
    for (var, g) in &generalized {
        all_schemes.insert(*var, g.scheme.clone());
    }
    let mut lowerer = core::Lowerer::new(
        &prims,
        &names,
        &effect_ops,
        &table,
        &ctor_arity,
        &generalized,
        &all_schemes,
        resolver.var_gen(),
    );
    lowerer.variants = Some(&variants);
    lowerer.evidence = Some(&evidence);
    lowerer.traits = Some(&traits);
    let mut defs = Vec::new();
    for m in &typed {
        lowerer.locations = opts.debug_info.then_some(m.source.id);
        let from = defs.len();
        defs.extend(lowerer.lower_module(&m.hir));
        // Which module each is of, package first: what the native back end
        // divides the program by.
        let module = InternedString::from(
            std::iter::once(pkg.to_string())
                .chain(m.path.iter().map(|p| p.to_string()))
                .collect::<Vec<_>>()
                .join(".")
                .as_str(),
        );
        for d in &mut defs[from..] {
            d.module = module;
        }
    }
    let var_end = lowerer.var_end();

    phase.done("to core");
    // --- what inference left unsolved, defaulted: see `core::defaults`.
    let binder_kinds: HashMap<VarId, Vec<meadow_infer::VarKind>> = all_schemes
        .iter()
        .map(|(v, s)| (*v, s.quant.clone()))
        .collect();
    core::defaults::defs(&mut defs, &binder_kinds);

    // --- lint (debug builds and tests only)
    //
    // Core is typed so that a pass over it can be checked; this is where the
    // checking happens. A failure is a compiler bug, not a program error, so
    // it is loud, and it costs a release build nothing.
    //
    // Only for a unit that compiled cleanly: the core of a program with type
    // errors in it is ill-typed, and saying so twice helps nobody.
    if cfg!(debug_assertions) && diags.is_empty() {
        let program = core::Program {
            defs: defs.clone(),
            entry: None,
            ctor_fields: lowerer.ctor_fields.clone(),
            variants: variants.clone(),
            origins: Default::default(),
        };
        let imported: HashMap<VarId, Scheme> = dep_schemes.iter().cloned().collect();
        let problems = core::lint::check(&program, &variants, &imported);
        assert!(
            problems.is_empty(),
            "core lint failed after lowering `{filename}`:\n{}",
            problems.join("\n")
        );
    }
    // Drop the `&table` borrow held by `lowerer` before `table` is moved below.
    let ctor_fields = lowerer.ctor_fields;

    // `@macro`: what a package offers as a procedural macro. Its type is what
    // makes it runnable, and it is checked here -- where the macro is written,
    // rather than in whoever imports it and finds out the hard way.
    let macro_vars: HashMap<VarId, meadow_span::Span> =
        resolver.macro_vars().iter().copied().collect();
    for (var, span) in &macro_vars {
        let Some(scheme) = schemes.get(var) else {
            continue;
        };
        if let Err(why) = crate::expand::proc::signature(scheme) {
            diags.push(Diagnostic {
                msg: format!("this cannot be a macro: {why}"),
                filename: filename.clone(),
                label: ("a macro is `[TokenTree] -> [TokenTree]`".to_string(), *span),
                extra_labels: vec![],
            });
        }
    }

    // Export surface. If the unit used a visibility attribute anywhere, only the `@pub`
    // declarations (and `@pub use` re-exports) are exported; otherwise everything.
    let gated = resolver.has_pub_markers();
    // Which module each top-level `VarId` was declared in (for `Export.module`).
    let mut var_module: HashMap<VarId, Vec<InternedString>> = HashMap::new();
    for m in &typed {
        collect_toplevel_vars(&m.hir, &m.path, &mut var_module);
    }
    let mut exports: Vec<Export> = Vec::new();
    let mut exported: std::collections::HashSet<VarId> = std::collections::HashSet::new();
    // An `impl`'s dictionary is found by type and never by name, so it is
    // exported whatever is marked; so is a default method, which an `impl` in
    // a dependent is built from.
    let hidden: std::collections::HashSet<VarId> =
        resolver.hidden_exports().iter().copied().collect();
    for (var, scheme) in &schemes {
        if gated && !resolver.is_pub_var(*var) && !hidden.contains(var) {
            continue;
        }
        exported.insert(*var);
        exports.push(Export {
            name: names.get(var).copied().unwrap_or_default(),
            var: *var,
            scheme: scheme.clone(),
            module: var_module.get(var).cloned().unwrap_or_default(),
            is_macro: macro_vars.contains_key(var),
        });
    }
    // `@pub use M (x)` re-exports: `x` is defined in a dependency, so its scheme
    // comes from `dep_schemes`. These carry an empty module path — they are the
    // package's unqualified (prelude) surface.
    if gated {
        let dep_scheme_map: HashMap<VarId, Scheme> = dep_schemes.iter().cloned().collect();
        for var in resolver.pub_var_ids() {
            if exported.contains(&var) {
                continue;
            }
            if let Some(scheme) = dep_scheme_map.get(&var) {
                exports.push(Export {
                    name: names.get(&var).copied().unwrap_or_default(),
                    var,
                    scheme: scheme.clone(),
                    module: Vec::new(),
                    is_macro: macro_vars.contains_key(&var),
                });
            }
        }
    }
    exports.sort_by_key(|e| e.var.0);

    let data_decls: Vec<hir::LDecl> = typed
        .iter()
        .flat_map(|m| m.hir.value().decls.iter())
        .filter(|d| match d.value() {
            hir::Decl::Data(dd) => !gated || resolver.is_pub_type(dd.name),
            hir::Decl::Record(rd) => !gated || resolver.is_pub_type(rd.name),
            hir::Decl::Effect(ed) => !gated || resolver.is_pub_type(ed.name),
            hir::Decl::Alias(ad) => !gated || resolver.is_pub_type(ad.name),
            hir::Decl::EffectAlias(ad) => !gated || resolver.is_pub_type(ad.name),
            hir::Decl::Trait(td) => !gated || resolver.is_pub_type(td.name),
            // Coherence is the program's: an `impl` is every dependent's.
            hir::Decl::Impl(_) => true,
            _ => false,
        })
        .map(without_bodies)
        .collect();

    // Constructors a dependent may write bare: only what a `@pub use M.Ty.*`
    // re-exports, which is the package saying "this is part of my unqualified
    // surface" -- exactly what the prelude does for `Maybe`, `Result` and
    // `Ordering`. A package that marks nothing exports every *name*, but its
    // constructors stay under their types like anyone's.
    flat_ctors.sort_by_key(|n| n.to_string());
    flat_ctors.dedup();
    flat_types.sort_by_key(|n| n.to_string());
    flat_types.dedup();

    // What a macro wrote is reported at the call, which is the only place in
    // the file there is to point at. This is what says which macro.
    crate::expand::blame(&mut diags, &expansions);

    (
        CompiledPackage {
            id,
            flat_ctors,
            flat_types,
            vars: var_base..var_end,
            name: unit_name,
            ident,
            macros,
            bindings,
            embedded: resolver.embedded().to_vec(),
            fixities: resolver.fixities(),
            entry,
            value_entry,
            modules: typed,
            types: table,
            exports,
            generalized: generalized
                .into_iter()
                .map(|(var, g)| (var, g.scheme))
                .collect(),
            defs,
            ctor_fields,
            variants,
            data_decls,
            tests: resolver.test_vars().to_vec(),
            prelude_exports: None,
            package: false,
            compacting: own_compacting,
        },
        diags,
    )
}

/// Reorder `items` so that the element at `order[k]` ends up `k`th.
/// A declaration as a dependent needs it: a trait's defaults and an `impl`'s
/// methods are compiled already, and only their names and types travel.
fn without_bodies(d: &hir::LDecl) -> hir::LDecl {
    let mut d = d.clone();
    match &mut *d.value {
        hir::Decl::Trait(td) => {
            for m in &mut td.methods {
                if let Some(default) = &mut m.default {
                    default.body = None;
                }
            }
        }
        hir::Decl::Impl(id) => id.methods.clear(),
        _ => {}
    }
    d
}

fn permute<T>(items: Vec<T>, order: &[usize]) -> Vec<T> {
    debug_assert_eq!(
        items.len(),
        order.len(),
        "permutation must cover every item"
    );
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    order
        .iter()
        .map(|&i| slots[i].take().expect("each index appears once"))
        .collect()
}

/// Lift every `mod Foo { … }` out of `modules` into a module of its own,
/// beside the ones that came from files, leaving `mod Foo` where it was
/// written -- attributes and all, so a `@pub` or a `@cfg` on it means what it
/// does on a module that has a file. A lifted module is lifted from in turn,
/// so they nest. Returns, for each module added, the place in `modules` of
/// the one it was written in.
pub(crate) fn lift_inline_modules(modules: &mut Vec<AstModule>) -> Vec<usize> {
    let mut parents = Vec::new();
    let mut i = 0;
    while i < modules.len() {
        let mut lifted = Vec::new();
        for d in &mut modules[i].ast.value.decls {
            let span = d.span;
            let slot = match &mut *d.value {
                ast::Decl::Attributed(_, inner) => &mut *inner.value,
                other => other,
            };
            if let ast::Decl::Module(name, decls) = slot {
                let name = name.clone();
                let decls = std::mem::take(decls);
                *slot = ast::Decl::Mod(name.clone());
                lifted.push((name, decls, span));
            }
        }
        for (name, decls, span) in lifted {
            let mut path = modules[i].path.clone();
            path.push(*name.value());
            modules.push(AstModule {
                path,
                name: *name.value(),
                ast: ast::LModule::new(
                    ast::Module {
                        name: *name.value(),
                        decls,
                        attrs: Vec::new(),
                    },
                    span,
                ),
                source: modules[i].source,
            });
            parents.push(i);
        }
        i += 1;
    }
    parents
}

/// The children `module` declares with `mod`, attributed or not.
fn declared_mods(module: &ast::Module) -> std::collections::BTreeSet<InternedString> {
    module
        .decls
        .iter()
        .filter_map(|d| {
            let base = match d.value() {
                ast::Decl::Attributed(_, inner) => inner.value(),
                other => other,
            };
            match base {
                ast::Decl::Mod(name) => Some(*name.value()),
                _ => None,
            }
        })
        .collect()
}

/// Record which module each top-level binding lives in.
fn collect_toplevel_vars(
    module: &hir::LModule,
    path: &[InternedString],
    out: &mut HashMap<VarId, Vec<InternedString>>,
) {
    for decl in &module.value().decls {
        match decl.value() {
            hir::Decl::Bind(bind) => {
                for id in bind.bound_vars() {
                    out.entry(id).or_insert_with(|| path.to_vec());
                }
            }
            // An effect's operations and a trait's methods are the module's
            // values too, known by its path as the rest are.
            hir::Decl::Effect(ed) => {
                for (_, id, _) in &ed.ops {
                    out.entry(*id.value()).or_insert_with(|| path.to_vec());
                }
            }
            hir::Decl::Trait(td) => {
                for m in &td.methods {
                    out.entry(*m.var.value()).or_insert_with(|| path.to_vec());
                }
            }
            _ => {}
        }
    }
}

/// Bring what a `use` names into scope, and report it if there is no such
/// module (or no such exported name).
///
/// The path ends in a module -- `use M`, `use M as C`, `use M (a, b)` -- or in a
/// type declared by one: `use M.Ty` for the type, `use M.Ty (A, B)` or
/// `use M.Ty.*` for its constructors unqualified. Returns the canonical
/// constructors that last form brought, which a `@pub use` re-exports flat,
/// and the types, effects and traits it named -- which it re-exports too.
fn apply_use(
    resolver: &mut Resolver,
    pkg: InternedString,
    u: &ast::UseDecl,
    deps: &[Dep<'_>],
    filename: &str,
    diags: &mut Vec<Diagnostic>,
) -> (Vec<InternedString>, Vec<InternedString>) {
    let mut types_brought: Vec<InternedString> = Vec::new();
    let ctors = use_in(resolver, pkg, u, deps, filename, diags, &mut types_brought);
    (ctors, types_brought)
}

fn use_in(
    resolver: &mut Resolver,
    pkg: InternedString,
    u: &ast::UseDecl,
    deps: &[Dep<'_>],
    filename: &str,
    diags: &mut Vec<Diagnostic>,
    types_brought: &mut Vec<InternedString>,
) -> Vec<InternedString> {
    // `use super (…)`: the module this one is written in, as Rust's
    // `super::`; `use super.super`, the one above that; `use super.Sibling`,
    // a module beside this one. Read as the path from the package it stands
    // for, so everything below treats it as if that had been written.
    let from_above;
    let u = if u.path.first().is_some_and(|s| &**s.value() == "super") {
        let ups = u
            .path
            .iter()
            .take_while(|s| &**s.value() == "super")
            .count();
        let here = resolver.current_module().to_vec();
        if ups > here.len() {
            diags.push(Diagnostic {
                msg: "there is no module above the package's root for `super` to name".to_string(),
                filename: filename.to_string(),
                label: ("above the root".to_string(), u.path[ups - 1].span),
                extra_labels: vec![],
            });
            return Vec::new();
        }
        let at = u.path[0].span;
        let mut path = vec![ast::Ident::new(pkg, at)];
        path.extend(
            here[..here.len() - ups]
                .iter()
                .map(|s| ast::Ident::new(*s, at)),
        );
        path.extend(u.path[ups..].iter().cloned());
        from_above = ast::UseDecl { path, ..u.clone() };
        &from_above
    } else {
        u
    };
    let segs: Vec<InternedString> = u.path.iter().map(|s| *s.value()).collect();
    if segs.is_empty() {
        return Vec::new();
    }
    // Every module the path passes through is a mention of it, for an editor
    // to follow: `Std`, `Std.Collections` and `Std.Collections.Vector`. By
    // package and path, a sibling's under this package's name. A segment that
    // is a type rather than a module names nothing an editor will find.
    let module_of = |prefix: &[InternedString]| -> InternedString {
        let local = if prefix[0] == pkg {
            &prefix[1..]
        } else {
            prefix
        };
        if prefix[0] == pkg || resolver.has_module(local) {
            let mut full = vec![pkg];
            full.extend_from_slice(local);
            InternedString::from(dotted(&full).as_str())
        } else {
            InternedString::from(dotted(prefix).as_str())
        }
    };
    let mentioned: Vec<(meadow_span::Span, InternedString)> = (1..=segs.len())
        .map(|i| (u.path[i - 1].span, module_of(&segs[..i])))
        .collect();
    let whole = module_of(&segs);
    for (span, m) in mentioned {
        resolver.note_ref(span, NameRef::Module(m));
    }
    // `use M (vec!)` selects a macro and nothing else. Expansion has already
    // taken it, and what is left must not be read as a bare `use M`, which
    // would bring in every name the module has.
    if u.names.is_empty() && !u.macros.is_empty() && !u.glob {
        return Vec::new();
    }
    let path_span = u
        .path
        .iter()
        .fold(u.path[0].span, |acc, s| acc.extend(s.span));
    let mut report = |msg: String, label: &str, span| {
        diags.push(Diagnostic {
            msg,
            filename: filename.to_string(),
            label: (label.to_string(), span),
            extra_labels: vec![],
        })
    };

    // `use Pack.Mod` — a sibling namespace of this very unit. The package is
    // the compilation unit and modules are namespaces inside it, so a sibling
    // is reached through the resolver's frames, not through `deps`.
    let local: Vec<InternedString> = if segs[0] == pkg {
        segs[1..].to_vec()
    } else {
        segs.clone()
    };
    let (ty, owner) = u.path.split_last().expect("a use path is never empty");
    let local_owner = &local[..local.len().saturating_sub(1)];

    // `use Ty ...` for a type this very module declares -- Rust's
    // `use self::Ty::*`, which is how a module writes its own constructors bare.
    // Before modules, so a type named like its module -- `Maybe` in
    // `Std.Maybe` -- is the type.
    if segs.len() == 1 {
        let here = resolver.current_module().to_vec();
        if resolver.module_has_type(&here, *ty.value()) {
            if let Some(a) = &u.alias {
                report(
                    format!("a type cannot be renamed with `as`"),
                    "not a module",
                    a.span,
                );
                return Vec::new();
            }
            return resolver.use_type(&here, ty, &u.names, u.glob);
        }
    }

    if u.glob && !local.is_empty() && resolver.has_module(&local) {
        let path = dotted(&segs);
        report(
            format!("`use {path}.*` names a module; `.*` is for a type's constructors"),
            "a module",
            path_span,
        );
        return Vec::new();
    }
    // `use Pack` alone is the package's root module -- Rust's `use crate::…`
    // -- whose path is empty.
    let names_module = !local.is_empty() || segs[0] == pkg;
    if names_module && resolver.has_module(&local) {
        match &u.alias {
            Some(a) => {
                if let Some((before, at)) = resolver.use_alias(*a.value(), whole, a.span) {
                    diags.push(Diagnostic {
                        msg: format!(
                            "`{}` already names `{before}` here, so it cannot name `{whole}` too",
                            a.value()
                        ),
                        filename: filename.to_string(),
                        label: ("a second module of that name".to_string(), a.span),
                        extra_labels: vec![("the first".to_string(), at)],
                    });
                    return Vec::new();
                }
                let values = resolver.module_values(&local);
                resolver.activate_module(*a.value(), values);
                resolver.activate_local_types(*a.value(), &local);
                resolver.qualifier_names(*a.value(), whole);
                resolver.note_ref(a.span, NameRef::Module(whole));
            }
            None => types_brought.extend(resolver.use_module(&local, &u.names)),
        }
        return Vec::new();
    }

    // `use Pack.Mod.Ty ...` for a sibling's type.
    if !local.is_empty() && resolver.module_has_type(local_owner, *ty.value()) {
        if let Some(a) = &u.alias {
            report(
                format!("a type cannot be renamed with `as`"),
                "not a module",
                a.span,
            );
            return Vec::new();
        }
        return resolver.use_type(local_owner, ty, &u.names, u.glob);
    }

    let Resolved { map, found } = resolve_module(pkg, &segs, deps);

    if !found {
        // `use Ty.*` / `use Ty (C)` for a dependency's type already in scope by
        // name -- an earlier REPL line's, say. Plain `use Ty` would bring nothing
        // new, and is far more likely a module path gone wrong, which the error
        // below can help with.
        if segs.len() == 1 && (u.glob || !u.names.is_empty()) {
            let known = resolver.imported_type_names();
            let spelled: Vec<InternedString> = resolver
                .types_spelled(*ty.value())
                .into_iter()
                .filter(|c| known.contains(c))
                .collect();
            match spelled.as_slice() {
                [canonical] => {
                    if !u.glob && u.names.is_empty() {
                        types_brought.push(*canonical);
                    }
                    return resolver.use_dep_type(ty, *canonical, &u.names, u.glob);
                }
                [] => {}
                many => {
                    let owners: Vec<String> = many
                        .iter()
                        .map(|c| format!("`{}`", hir::type_package(c).unwrap_or("Std")))
                        .collect();
                    report(
                        format!(
                            "`{}` could be the type of any of {}; write the package's path, \
                             as in `use pkg.{}.*`",
                            ty.value(),
                            owners.join(", "),
                            ty.value()
                        ),
                        "ambiguous type",
                        ty.span,
                    );
                    return Vec::new();
                }
            }
        }
        // `use Pkg.Mod.Ty ...` for a dependency's type.
        if segs.len() > 1 {
            let owner_segs: Vec<InternedString> = owner.iter().map(|s| *s.value()).collect();
            let canonical = module_types(pkg, &owner_segs, deps)
                .into_iter()
                .find(|t| hir::spelling(t) == &**ty.value());
            if let (true, Some(canonical)) =
                (resolve_module(pkg, &owner_segs, deps).found, canonical)
            {
                if let Some(a) = &u.alias {
                    report(
                        format!("a type cannot be renamed with `as`"),
                        "not a module",
                        a.span,
                    );
                    return Vec::new();
                }
                if !u.glob && u.names.is_empty() {
                    types_brought.push(canonical);
                }
                return resolver.use_dep_type(ty, canonical, &u.names, u.glob);
            }
        }
        let path = dotted(&segs);
        let mut msg = format!("no module `{path}`");
        if let Some(suggestion) = suggest_module(&segs, deps) {
            msg.push_str(&format!(" — did you mean `{suggestion}`?"));
        }
        report(msg, "not found", path_span);
        return Vec::new();
    }
    if u.glob {
        let path = dotted(&segs);
        report(
            format!("`use {path}.*` names a module; `.*` is for a type's constructors"),
            "a module",
            path_span,
        );
        return Vec::new();
    }

    // A package's root re-exports constructors flat with `@pub use M.Ty.*`,
    // as a Rust crate root does with `pub use Ty::*`: `use pkg` brings them
    // all, and `use pkg (C)` the ones it names. And the types it names in a
    // `@pub use`, likewise.
    let flat: Vec<InternedString> = if segs.len() == 1 {
        deps.iter()
            .filter(|d| d.spelled == segs[0] && d.prelude_exports.is_none())
            .flat_map(|d| d.flat_ctors.iter().copied())
            .collect()
    } else {
        Vec::new()
    };
    let flat_types: Vec<InternedString> = if segs.len() == 1 {
        deps.iter()
            .filter(|d| d.spelled == segs[0] && d.prelude_exports.is_none())
            .flat_map(|d| d.flat_types.iter().copied())
            .collect()
    } else {
        Vec::new()
    };
    let flat_named = |name: InternedString| {
        flat.iter()
            .copied()
            .find(|c| c.rsplit_once('.').map_or(&**c, |(_, b)| b) == &*name)
    };
    if u.alias.is_none() && u.names.is_empty() {
        for c in &flat {
            resolver.use_flat_ctor(*c);
        }
        for t in &flat_types {
            resolver.use_named_type(*t);
            types_brought.push(*t);
        }
        // `use M`: its types, effects and traits too, as its values.
        for t in module_types(pkg, &segs, deps) {
            resolver.use_named_type(t);
            types_brought.push(t);
        }
    }

    match &u.alias {
        // `use M as C` — a qualifier and nothing else. `C.name` reaches the
        // module's exports; none of them are in scope unqualified.
        Some(a) => {
            if let Some((before, at)) = resolver.use_alias(*a.value(), whole, a.span) {
                diags.push(Diagnostic {
                    msg: format!(
                        "`{}` already names `{before}` here, so it cannot name `{whole}` too",
                        a.value()
                    ),
                    filename: filename.to_string(),
                    label: ("a second module of that name".to_string(), a.span),
                    extra_labels: vec![("the first".to_string(), at)],
                });
                return Vec::new();
            }
            resolver.activate_module(*a.value(), map.clone());
            resolver.activate_module_types(*a.value(), &module_types(pkg, &segs, deps));
            resolver.qualifier_names(*a.value(), whole);
            resolver.note_ref(a.span, NameRef::Module(whole));
        }
        // `use M` — bring every exported name into scope unqualified. Qualifying
        // is what `as` is for, so a bare `use` does not introduce one.
        None if u.names.is_empty() => {
            // Sorted, so the scope is built the same way on every run.
            let mut all: Vec<(InternedString, VarId)> =
                map.iter().map(|(n, id)| (*n, *id)).collect();
            all.sort_by_key(|(n, _)| n.to_string());
            let from = InternedString::from(dotted(&segs));
            for (name, id) in all {
                resolver.import_from(name, id, from);
            }
            // And its effects' operations, which are values too.
            for (_, op, id) in module_effect_ops(pkg, &segs, deps) {
                resolver.import_from(op, id, from);
            }
        }
        None => {}
    }
    // An effect named brings its operations, as a trait named brings its
    // methods; an operation named brings itself.
    if u.alias.is_none() && !u.names.is_empty() {
        let from = InternedString::from(dotted(&segs));
        for (effect, op, id) in module_effect_ops(pkg, &segs, deps) {
            if u.names
                .iter()
                .any(|n| *n.value() == op || &**n.value() == hir::spelling(&effect))
            {
                resolver.import_from(op, id, from);
            }
        }
    }
    // Only *values* live in `map`. A selected name can also be a type or an
    // effect operation, and those are imported wholesale elsewhere
    // (`import_types`), so a miss here is not an error -- unless it is a
    // constructor, which lives under its type and is never a module's item.
    let types = module_types(pkg, &segs, deps);
    for n in &u.names {
        let name = *n.value();
        // `P`, a pattern synonym: its matcher and its builder, which are what
        // it is. Sorted, so the scope is built the same way on every run.
        let mut parts: Vec<(InternedString, VarId)> = map
            .iter()
            .filter(|(f, _)| meadow_rename::part_of_synonym(f, &name))
            .map(|(f, id)| (*f, *id))
            .collect();
        parts.sort_by_key(|(f, _)| f.to_string());
        let synonym = !parts.is_empty();
        for (f, id) in parts {
            resolver.import_from(f, id, InternedString::from(dotted(&segs)));
        }
        if let Some(&id) = map.get(&name) {
            resolver.import_from(name, id, InternedString::from(dotted(&segs)));
            resolver.note_ref(n.span, NameRef::Value(id));
        } else if synonym {
        } else if let Some(methods) = trait_methods(pkg, &segs, name, deps) {
            // A trait brings its methods: naming `Show` is asking to call
            // `show`. And itself, to be asked for and implemented.
            let from = InternedString::from(dotted(&segs));
            for m in methods {
                if let Some(&id) = map.get(&m) {
                    resolver.import_from(m, id, from);
                }
            }
            // The trait by its fully qualified name: a type is known by nothing
            // less.
            let canonical = types.iter().copied().find(|t| hir::spelling(t) == &*name);
            if let Some(canonical) = canonical {
                resolver.use_named_type(canonical);
                types_brought.push(canonical);
            }
            resolver.note_ref(n.span, NameRef::Type(canonical.unwrap_or(name)));
        } else if let Some(&canonical) = types.iter().find(|t| hir::spelling(t) == &*name) {
            // Naming a type in a `use` is what settles which one a spelling
            // shared by several packages means.
            resolver.use_named_type(canonical);
            types_brought.push(canonical);
            resolver.note_ref(n.span, NameRef::Type(canonical));
        } else if let Some(c) = flat_named(name) {
            resolver.use_flat_ctor(c);
        } else if let Some(&t) = flat_types.iter().find(|t| hir::spelling(t) == &*name) {
            resolver.use_named_type(t);
            types_brought.push(t);
        } else if let Some(owner) = types
            .iter()
            .find(|t| resolver.type_ctor_names(**t).contains(&name))
        {
            let module = dotted(&segs);
            let owner = hir::spelling(owner);
            report(
                format!("`{name}` is a constructor of `{owner}`, not an item of `{module}`"),
                &format!("write `use {module}.{owner} ({name})`, or `{owner}.{name}`"),
                n.span,
            );
        } else if name.chars().next().is_some_and(|c| c.is_uppercase()) {
            // An effect: no id to carry, so it is named.
            resolver.note_ref(n.span, NameRef::Type(name));
        }
    }
    Vec::new()
}

/// The methods of the trait `name`, if a dependency the `use` path `segs`
/// reaches declares one by that name.
fn trait_methods(
    pkg: InternedString,
    segs: &[InternedString],
    name: InternedString,
    deps: &[Dep<'_>],
) -> Option<Vec<InternedString>> {
    // A module of this package compiled as a unit of its own is named by its
    // path without the package, as `module_types` reads it.
    let local: &[InternedString] = if segs.first() == Some(&pkg) {
        &segs[1..]
    } else {
        segs
    };
    deps.iter()
        .filter(|d| {
            segs.first() == Some(&d.spelled)
                || dotted(segs) == *d.spelled.to_string()
                || dotted(local) == *d.spelled.to_string()
        })
        .flat_map(|d| d.data_decls.iter())
        .find_map(|decl| match decl.value() {
            hir::Decl::Trait(td) if hir::spelling(&td.name) == &*name => {
                Some(td.methods.iter().map(|m| m.name).collect())
            }
            _ => None,
        })
}

/// The exported types a dependency module declares: its `data`, `record` and
/// `type` declarations, its effects and its traits.
fn module_types(
    pkg: InternedString,
    segs: &[InternedString],
    deps: &[Dep<'_>],
) -> Vec<InternedString> {
    let names = |decls: &mut dyn Iterator<Item = &hir::LDecl>| -> Vec<InternedString> {
        decls
            .filter_map(|d| match d.value() {
                hir::Decl::Data(dd) => Some(dd.name),
                hir::Decl::Record(rd) => Some(rd.name),
                hir::Decl::Alias(ad) => Some(ad.name),
                hir::Decl::EffectAlias(ad) => Some(ad.name),
                hir::Decl::Effect(ed) => Some(ed.name),
                hir::Decl::Trait(td) => Some(td.name),
                _ => None,
            })
            .collect()
    };
    let local: &[InternedString] = if segs.first() == Some(&pkg) {
        &segs[1..]
    } else {
        segs
    };
    let mut out = Vec::new();
    for dep in deps {
        let exported = names(&mut dep.data_decls.iter());
        // The module's path inside `dep`: a separately compiled sub-module is
        // named by its own dotted path, and an external package by its first
        // segment.
        let wants: Vec<&[InternedString]> = [
            (dotted(local) == *dep.spelled.to_string()).then_some(local),
            (segs.first() == Some(&dep.spelled)).then(|| &segs[1..]),
        ]
        .into_iter()
        .flatten()
        .collect();
        for m in dep
            .modules
            .iter()
            .filter(|m| wants.contains(&m.path.as_slice()))
        {
            out.extend(
                names(&mut m.hir.value().decls.iter())
                    .into_iter()
                    .filter(|t| exported.contains(t)),
            );
        }
    }
    out
}

/// The operations of the exported effects a dependency module declares: the
/// effect, the operation, and the operation's value.
fn module_effect_ops(
    pkg: InternedString,
    segs: &[InternedString],
    deps: &[Dep<'_>],
) -> Vec<(InternedString, InternedString, VarId)> {
    let effects = module_types(pkg, segs, deps);
    let local: &[InternedString] = if segs.first() == Some(&pkg) {
        &segs[1..]
    } else {
        segs
    };
    let mut out = Vec::new();
    for dep in deps {
        let wants: Vec<&[InternedString]> = [
            (dotted(local) == *dep.spelled.to_string()).then_some(local),
            (segs.first() == Some(&dep.spelled)).then(|| &segs[1..]),
        ]
        .into_iter()
        .flatten()
        .collect();
        for m in dep
            .modules
            .iter()
            .filter(|m| wants.contains(&m.path.as_slice()))
        {
            for d in &m.hir.value().decls {
                if let hir::Decl::Effect(ed) = d.value()
                    && effects.contains(&ed.name)
                {
                    for (op, id, _) in &ed.ops {
                        out.push((ed.name, *op, *id.value()));
                    }
                }
            }
        }
    }
    out
}

/// What a `use` path names: whether the module exists at all, and the values it
/// exports.
pub struct Resolved {
    /// Exported **values**, by name. Types, constructors and effect operations
    /// are imported wholesale elsewhere and never appear here.
    pub map: HashMap<InternedString, VarId>,
    /// Whether the module exists — which is not the same as `!map.is_empty()`,
    /// since `Std.Collections` is nothing but `mod` declarations.
    pub found: bool,
}

/// Look up the module a `use` path names among `deps`. Shared by `use`
/// resolution and by the REPL's completer, so the two cannot disagree about
/// what is in scope.
pub fn resolve_module(pkg: InternedString, segs: &[InternedString], deps: &[Dep<'_>]) -> Resolved {
    let mut map = HashMap::new();
    let mut found = false;
    if segs.is_empty() {
        return Resolved { map, found };
    }
    // `use Pkg.a.b` inside package `Pkg` refers to the local module `a.b`.
    let local: &[InternedString] = if segs[0] == pkg { &segs[1..] } else { segs };

    for dep in deps {
        // intra-batch sub-module: dep is named by its dotted module path
        let dep_is_local_module =
            dotted(local) == &*dep.spelled.to_string() || dotted(segs) == &*dep.spelled.to_string();
        if dep_is_local_module {
            found = true;
            for e in &dep.exports {
                map.insert(e.name, e.var);
            }
            continue;
        }
        // external package: first segment is the package name, rest is module path
        if segs[0] == dep.spelled {
            let want = &segs[1..];
            if dep.modules.iter().any(|m| m.path == want) {
                found = true;
            }
            for e in &dep.exports {
                if e.module == want {
                    map.insert(e.name, e.var);
                }
            }
        }
    }
    Resolved { map, found }
}

/// A module elsewhere whose last segment matches the one asked for, rendered as
/// the full path it should have been written as — so `use List` can point at
/// `Std.Collections.List`.
fn suggest_module(segs: &[InternedString], deps: &[Dep<'_>]) -> Option<String> {
    let last = segs.last()?;
    for dep in deps {
        for m in &dep.modules {
            if m.path.last() == Some(last) && m.path.as_slice() != segs {
                let mut full = vec![dep.spelled];
                full.extend(m.path.iter().copied());
                return Some(dotted(&full));
            }
        }
    }
    None
}

/// What a diagnostic from `source` should call its file.
///
/// A unit is compiled as a whole, but its modules are separate files and an
/// error has to name the one it is in. Interactive text — the REPL, a
/// `compile_str` — has no file, so it keeps the unit's name.
pub(crate) fn module_filename(unit: &str, source: Source) -> String {
    match source.kind {
        SourceKind::File(name) => name.to_string(),
        SourceKind::Interactive => unit.to_string(),
    }
}

fn dotted(segs: &[InternedString]) -> String {
    segs.iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

fn prim_map(resolver: &Resolver) -> HashMap<VarId, core::Prim> {
    resolver
        .prelude_bindings()
        .into_iter()
        .filter_map(|(name, id)| core::Prim::from_name(&name).map(|p| (id, p)))
        .collect()
}
