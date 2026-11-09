//! The **build driver**: filesystem package discovery, running
//! [`meadow_compiler::compile_unit`] over the package graph, linking the result,
//! and injecting the embedded `Std` package.
//!
//! The per-unit compile (resolve → infer → lower) itself lives in
//! `meadow-compiler`; [`compile_unit`] / [`compile_str`] / [`CompiledPackage`] are
//! re-exported here for convenience.

use crate::incremental::{self, Cache};
use crate::linker::{LinkedProgram, Linker};
use crate::package::{Package, PackageGraph};
use crate::status;
use crate::stdlib;
use crate::workspace::Workspace;
use chumsky::error::Rich;
use meadow_compiler::{
    Options, core,
    diagnostics::Diagnostic,
    intern::InternedString,
    lexer::{Token, tokenize},
    parser,
    source::{Source, SourceKind},
    span::Span,
};
use std::path::Path;

pub use meadow_compiler::{
    AstModule, CompiledPackage, Export, TypedModule, compile_str, compile_unit,
};

pub struct BuildOutput {
    pub linked: Option<LinkedProgram>,
    pub diagnostics: Vec<Diagnostic>,
    /// The package that was built: the directory [`crate::artifacts`] puts
    /// its `target` under -- its own, or its workspace's root -- and its name.
    pub package: Option<(std::path::PathBuf, InternedString)>,
    /// The packages that were compiled, in the order they were: every other
    /// package the build needed was the same as last time, and read back
    /// instead -- see [`crate::incremental`].
    pub compiled: Vec<InternedString>,
    /// The built package's fingerprint, which is of everything it was compiled
    /// from -- its dependencies', the compiler's and the options' included:
    /// what an executable made of it is known by, before anything is lowered.
    /// `None` for a build of anything but what is on disk.
    pub fingerprint: Option<u64>,
}

/// Discover, compile and link the package rooted at `entry`.
///
/// The embedded `Std` package (see [`crate::stdlib`]) is compiled first and made
/// an implicit dependency of every package, so the prelude is always in scope.
pub fn build(entry: &Path, opts: Options) -> BuildOutput {
    build_with(entry, opts, None)
}

/// [`build`], resolving dependencies with `resolver` rather than the one the
/// command line set up.
pub fn build_resolved(
    entry: &Path,
    opts: Options,
    resolver: &mut crate::package::Resolver,
) -> BuildOutput {
    build_inner(entry, opts, None, Some(resolver))
}

/// Text to add to the end of one module before it is compiled -- how a
/// debugger starts a program at a function instead of `main`: it appends a
/// definition that calls it, in the function's own module so that everything
/// the function can see, the definition can too.
pub struct Addition<'a> {
    /// The module's file.
    pub file: &'a Path,
    pub text: &'a str,
}

/// [`build`], with `addition` appended to one of the modules first.
///
/// Diagnostics from the added text point past the end of the file, which is
/// how a caller can tell them from the file's own.
pub fn build_with(entry: &Path, opts: Options, addition: Option<Addition<'_>>) -> BuildOutput {
    build_inner(entry, opts, addition, None)
}

fn build_inner(
    entry: &Path,
    opts: Options,
    addition: Option<Addition<'_>>,
    resolver: Option<&mut crate::package::Resolver>,
) -> BuildOutput {
    let found = match resolver {
        Some(r) => discover_with(&[entry], r),
        None => discover(&[entry]),
    };
    let mut graph = match found {
        Ok(g) => g,
        Err(d) => return BuildOutput::failed(d),
    };

    let adding = addition.is_some();
    if let Some(add) = addition {
        let want = std::fs::canonicalize(add.file).unwrap_or_else(|_| add.file.to_path_buf());
        let module = graph
            .packages
            .iter_mut()
            .flat_map(|p| p.modules.iter_mut())
            .find(|m| match m.source.kind {
                SourceKind::File(name) => {
                    let p = Path::new(&*name);
                    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()) == want
                }
                SourceKind::Interactive => false,
            });
        let Some(module) = module else {
            return BuildOutput::failed(Diagnostic {
                msg: format!("{} is not a module of this package", add.file.display()),
                filename: add.file.display().to_string(),
                label: (String::new(), Span::from(0..0)),
                extra_labels: vec![],
            });
        };
        let text = format!("{}\n{}\n", &*module.source.content, add.text);
        module.source = Source::new(module.source.kind, InternedString::from(text));
    }

    let package = target_of(&graph, graph.root());
    // Text added for the debugger is not the package on disk, and is not
    // worth keeping.
    let incremental = match adding {
        false => cache_for(&package, opts),
        true => None,
    };
    let compiled = compile_graph(&graph, opts, incremental.as_ref());
    let fingerprint = (!adding).then(|| compiled.fingerprints[graph.root()]);
    let ordered = compiled
        .std
        .into_iter()
        .chain(compiled.packages.into_iter().map(|p| p.expect("compiled")))
        .collect();
    BuildOutput {
        linked: Some(Linker::link(ordered)),
        diagnostics: compiled.diagnostics,
        package,
        compiled: compiled.compiled,
        fingerprint,
    }
}

/// Every package a build of `entry` compiles, left unlinked: what a tool that
/// reads what the compiler made of each one wants -- MeadowBoot's differential
/// tests, say. Compiled afresh, with no cache read or written.
pub struct CompiledGraph {
    /// The standard library's modules, each a package of its own.
    pub std: Vec<CompiledPackage>,
    /// The graph's packages, each with its root directory; `None` where one
    /// could not be compiled because something it depends on was not.
    pub packages: Vec<(std::path::PathBuf, Option<CompiledPackage>)>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Compile the package at `entry` and everything it depends on -- see
/// [`CompiledGraph`] -- or say why its graph could not be found.
pub fn compile_packages(entry: &Path, opts: Options) -> Result<CompiledGraph, Diagnostic> {
    let graph = discover(&[entry])?;
    let compiled = compile_graph(&graph, opts, None);
    Ok(CompiledGraph {
        std: compiled.std,
        packages: graph
            .packages
            .iter()
            .map(|p| p.root.clone())
            .zip(compiled.packages)
            .collect(),
        diagnostics: compiled.diagnostics,
    })
}

/// Where a build whose `target` is `package`'s keeps compiled packages for
/// the next one: none for a lone file, which has no `target`.
fn cache_for(
    package: &Option<(std::path::PathBuf, InternedString)>,
    opts: Options,
) -> Option<Cache> {
    package
        .as_ref()
        .and_then(|(root, _)| Cache::new(root, opts))
}

impl BuildOutput {
    fn failed(d: Diagnostic) -> BuildOutput {
        BuildOutput {
            linked: None,
            diagnostics: vec![d],
            package: None,
            compiled: Vec::new(),
            fingerprint: None,
        }
    }
}

/// Several packages built at once -- a workspace's members -- sharing
/// everything they have in common: each package is compiled once however
/// many of them depend on it.
pub struct ManyOutput {
    /// Each package asked for, linked on its own with what it depends on, in
    /// the order asked; empty when the packages could not be found.
    pub each: Vec<BuildOutput>,
    /// Discovery and compile errors, for all of them: each of `each` has none.
    pub diagnostics: Vec<Diagnostic>,
    /// The packages compiled rather than reused, for all of them.
    pub compiled: Vec<InternedString>,
}

/// Build every package in `entries`, each linked into a program of its own.
pub fn build_each(entries: &[&Path], opts: Options) -> ManyOutput {
    let graph = match discover(entries) {
        Ok(g) => g,
        Err(d) => {
            return ManyOutput {
                each: Vec::new(),
                diagnostics: vec![d],
                compiled: Vec::new(),
            };
        }
    };
    let incremental = cache_for(&target_of(&graph, graph.root()), opts);
    let compiled = compile_graph(&graph, opts, incremental.as_ref());
    let each = graph
        .roots()
        .iter()
        .map(|&root| {
            let ordered = compiled
                .std
                .iter()
                .cloned()
                .chain(
                    graph
                        .closure(root)
                        .into_iter()
                        .map(|id| compiled.packages[id].clone().expect("compiled")),
                )
                .collect();
            BuildOutput {
                linked: Some(Linker::link(ordered)),
                diagnostics: Vec::new(),
                package: target_of(&graph, root),
                compiled: Vec::new(),
                fingerprint: Some(compiled.fingerprints[root]),
            }
        })
        .collect();
    ManyOutput {
        each,
        diagnostics: compiled.diagnostics,
        compiled: compiled.compiled,
    }
}

/// Build every package in `entries` into one program -- what testing several
/// at once runs -- answering it and the names of the packages asked for.
///
/// Its entry point is whichever `main` was linked last, so it is for running
/// tests, not for running.
pub fn build_together(entries: &[&Path], opts: Options) -> (BuildOutput, Vec<InternedString>) {
    let graph = match discover(entries) {
        Ok(g) => g,
        Err(d) => return (BuildOutput::failed(d), Vec::new()),
    };
    let names = graph
        .roots()
        .iter()
        .map(|&id| graph.packages[id].name)
        .collect();
    let package = target_of(&graph, graph.root());
    let incremental = cache_for(&package, opts);
    let compiled = compile_graph(&graph, opts, incremental.as_ref());
    let ordered = compiled
        .std
        .into_iter()
        .chain(compiled.packages.into_iter().map(|p| p.expect("compiled")))
        .collect();
    let out = BuildOutput {
        linked: Some(Linker::link(ordered)),
        diagnostics: compiled.diagnostics,
        package,
        compiled: compiled.compiled,
        fingerprint: None,
    };
    (out, names)
}

/// The graph of `entries` and everything they depend on -- or why there is
/// none.
fn discover(entries: &[&Path]) -> Result<PackageGraph, Diagnostic> {
    let lock_dir = crate::lock::dir_for(entries.first().copied().unwrap_or(Path::new(".")));
    let mut resolver = crate::package::Resolver::for_entry(&lock_dir);
    discover_with(entries, &mut resolver)
}

/// [`discover`], with a resolver of the caller's: how a test fetches into a
/// cache of its own, and how `meadow update` re-resolves.
fn discover_with(
    entries: &[&Path],
    resolver: &mut crate::package::Resolver,
) -> Result<PackageGraph, Diagnostic> {
    // A package inside a workspace it is not a member of would build with the
    // wrong profiles into the wrong `target`: say so instead.
    for entry in entries {
        if let Err(msg) = Workspace::find(entry) {
            return Err(Diagnostic {
                msg,
                filename: entry.display().to_string(),
                label: (String::new(), Span::from(0..0)),
                extra_labels: vec![],
            });
        }
    }
    // Dependencies are resolved against the lockfile, and what was resolved is
    // written back -- so a first build pins what it found, and every build
    // after it uses those commits until `meadow update` says otherwise.
    // And one its `meadow-toolchain` says is built with another `meadow`
    // than this is not built with this one.
    for entry in entries {
        if let Some(msg) = crate::channel::check_toolchain(entry) {
            return Err(Diagnostic {
                msg,
                filename: entry.display().to_string(),
                label: (String::new(), Span::from(0..0)),
                extra_labels: vec![],
            });
        }
    }
    let lock_dir = crate::lock::dir_for(entries.first().copied().unwrap_or(Path::new(".")));
    let graph = PackageGraph::build_all_with(entries, resolver)?;
    // Said before anything is compiled, so that a manifest on its way out is
    // seen whether or not the build goes on to succeed.
    for w in graph.warnings() {
        eprintln!("warning: {w}");
    }
    if !resolver.seen.is_empty() {
        resolver.lock.retain(&resolver.seen);
        if let Err(e) = resolver.lock.save(&lock_dir) {
            eprintln!("warning: could not write {}: {e}", crate::lock::FILE);
        }
    }

    // `Std` is embedded and injected below as an implicit dependency, so building
    // the tree on disk would declare every one of its names twice. Say that,
    // rather than emitting a hundred `already defined` errors that name the
    // symptom instead of the cause.
    if graph
        .packages
        .iter()
        .any(|p| &*p.name == stdlib::PACKAGE_NAME)
    {
        return Err(Diagnostic {
            msg: "`Std` is embedded in the compiler and cannot be built as a package \
                  — its modules are compiled one per unit, which a package build \
                  cannot reproduce. Run `meadow test --std` to run its tests"
                .to_string(),
            filename: stdlib::PACKAGE_NAME.to_string(),
            label: (String::new(), Span::from(0..0)),
            extra_labels: vec![],
        });
    }
    Ok(graph)
}

/// Where package `id` of `graph` keeps its `target` directory, and its name:
/// its own directory, or its workspace's root. A lone `.mw` file is compiled
/// as a package of one, but has no directory of its own to keep one in.
fn target_of(graph: &PackageGraph, id: usize) -> Option<(std::path::PathBuf, InternedString)> {
    let pkg = &graph.packages[id];
    if !pkg.root.is_dir() {
        return None;
    }
    let dir = match Workspace::find(&pkg.root) {
        Ok(Some(ws)) => ws.root,
        _ => pkg.root.clone(),
    };
    Some((dir, pkg.name))
}

/// Every package of a graph, compiled.
pub(crate) struct Compiled {
    pub(crate) std: Vec<CompiledPackage>,
    /// Indexed by package id.
    pub(crate) packages: Vec<Option<CompiledPackage>>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// Which were compiled rather than read back from the cache.
    pub(crate) compiled: Vec<InternedString>,
    /// Each package's fingerprint, by id: see [`incremental::fingerprint`].
    pub(crate) fingerprints: Vec<u64>,
}

/// Who each package of `graph` is, for naming the types and effects it
/// declares.
///
/// `name@version` tells two copies of one package apart, which is what makes
/// their types different types rather than one type with two definitions. Two
/// copies of the *same* version -- the same package from two places -- take
/// where they came from as well, since nothing else distinguishes them.
fn identities(graph: &PackageGraph) -> Vec<InternedString> {
    let plain: Vec<String> = graph.packages.iter().map(|p| p.ident()).collect();
    graph
        .packages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let shared = plain
                .iter()
                .enumerate()
                .any(|(j, other)| j != i && *other == plain[i]);
            if shared {
                let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                for b in p.origin.bytes() {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(0x0100_0000_01b3);
                }
                InternedString::from(format!("{}#{:08x}", plain[i], h as u32))
            } else {
                InternedString::from(plain[i].as_str())
            }
        })
        .collect()
}

/// Compile every package of `graph`, dependencies first -- reading back from
/// `cache` each one whose inputs are what they were when it was saved.
pub(crate) fn compile_graph(
    graph: &PackageGraph,
    opts: Options,
    cache: Option<&Cache>,
) -> Compiled {
    let (std, mut diagnostics) = stdlib::std_packages(opts);
    let n = graph.packages.len();
    let mut packages: Vec<Option<CompiledPackage>> = (0..n).map(|_| None).collect();
    let mut fingerprints = vec![0u64; n];
    // Compiled without a diagnostic, and so did everything under it: only
    // such a package is saved.
    let mut clean = vec![false; n];
    let mut compiled = Vec::new();
    // Each package's variables start above every package compiled before it,
    // not only above its own dependencies: two packages beside each other
    // would otherwise share ids, and one program holding both -- a package
    // depending on each, or a workspace -- would have one overwrite the other.
    // On a slot boundary, so that one package growing does not move the ids of
    // every package after it, which would make each of them a change.
    let mut floor = incremental::align(std.iter().map(|p| p.vars.end).max().unwrap_or(0));

    let idents = identities(graph);

    // What each package is built with of its `[features]`, which its
    // `@cfg(feature = "…")`s ask after.
    let features = match graph.enabled_features() {
        Ok(f) => f,
        Err(msg) => {
            diagnostics.push(Diagnostic {
                msg,
                filename: "Meadow.toml".to_string(),
                label: (String::new(), Span::from(0..0)),
                extra_labels: vec![],
            });
            vec![Vec::new(); n]
        }
    };

    // The bar counts every package in the graph. One found up to date moves it
    // on without a line of its own, as cargo does with a crate that is fresh.
    let mut bar = status::Building::new(graph.order().len());
    for &pid in graph.order() {
        let pkg = &graph.packages[pid];
        // `@cfg(test)` holds in the packages being tested, not in what they
        // depend on -- as cargo compiles a dependency without `cfg(test)`. A
        // dependency's tests are its own business, and need not even compile
        // under the compiler testing the package that uses it.
        let mut opts = opts;
        if !graph.roots().contains(&pid) {
            opts.cfg.test = false;
        }
        opts.cfg.flags = crate::package::feature_flags(opts.cfg.flags, &features[pid]);
        let dep_prints: Vec<u64> = pkg.deps.iter().map(|&d| fingerprints[d]).collect();
        let fingerprint = incremental::fingerprint(pkg, &dep_prints, opts, floor);
        // A build that writes out what macros produced has to run them: the
        // packages asked for are compiled, not read back.
        let expanding =
            meadow_compiler::expand::record::producing() && graph.roots().contains(&pid);
        let reused = cache
            .filter(|_| !expanding)
            .and_then(|c| c.load(&pkg.name, fingerprint));
        let cp = match reused {
            Some(mut cp) => {
                cp.id = pkg.id;
                clean[pid] = true;
                cp
            }
            None => {
                let version = pkg
                    .version
                    .as_deref()
                    .map(|v| format!(" v{v}"))
                    .unwrap_or_default();
                status::status(
                    "Compiling",
                    format!("{}{version} ({})", pkg.name, pkg.origin),
                );
                bar.working_on(&pkg.name);
                // The standard library is known by its own name; every other
                // dependency by the name *this* package calls it, which is what
                // its `use` lines say and need not be what it calls itself.
                let mut deps: Vec<meadow_compiler::Dep<'_>> =
                    std.iter().map(meadow_compiler::Dep::new).collect();
                deps.extend(pkg.deps.iter().zip(&pkg.dep_names).map(|(d, alias)| {
                    meadow_compiler::Dep::named(
                        *alias,
                        packages[*d].as_ref().expect("topological order"),
                    )
                }));
                // What a procedural macro would be run with: everything
                // compiled so far. Nothing is linked unless one is called.
                let mut runner = crate::proc::Macros::new(deps.iter().map(|d| d.pkg).collect());
                if let Some(cache) = cache {
                    runner = runner.remembering(
                        cache.macros(&pkg.name),
                        incremental::macros_print(&dep_prints),
                    );
                }
                let (cp, mut d) = compile_package(pkg, idents[pid], &deps, opts, floor, &runner);
                clean[pid] = d.is_empty() && pkg.deps.iter().all(|&d| clean[d]);
                diagnostics.append(&mut d);
                if clean[pid]
                    && let Some(cache) = cache
                {
                    cache.store(&cp, fingerprint);
                }
                compiled.push(pkg.name);
                cp
            }
        };
        fingerprints[pid] = fingerprint;
        floor = incremental::align(floor.max(cp.vars.end));
        packages[pid] = Some(cp);
        bar.step();
    }
    drop(bar);
    Compiled {
        std,
        packages,
        diagnostics,
        compiled,
        fingerprints,
    }
}

/// Where `decls` -- or a module written among them -- first says `use
/// Std.Ffi`, if they do.
fn uses_ffi(decls: &[meadow_compiler::ast::LDecl]) -> Option<Span> {
    use meadow_compiler::ast::Decl;
    decls.iter().find_map(|d| {
        let base = match d.value() {
            Decl::Attributed(_, inner) => inner.value(),
            other => other,
        };
        match base {
            Decl::Use(u)
                if u.path.len() >= 2
                    && &**u.path[0].value() == "Std"
                    && &**u.path[1].value() == "Ffi" =>
            {
                Some(u.path[1].span)
            }
            Decl::Module(_, inner) => uses_ffi(inner),
            _ => None,
        }
    })
}

fn compile_package(
    pkg: &Package,
    ident: InternedString,
    deps: &[meadow_compiler::Dep<'_>],
    opts: Options,
    floor: u32,
    procs: &dyn meadow_compiler::expand::proc::Runner,
) -> (CompiledPackage, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let mut modules = Vec::new();

    for m in &pkg.modules {
        let lex = tokenize(m.source);
        diags.extend(lex.errors);
        let (ast, perrs) = parser::parse(m.name, m.source, &lex.tokens);
        for e in &perrs {
            diags.push(rich_to_diag(&m.source, e));
        }
        if let Some(ast) = ast {
            modules.push(AstModule {
                path: m.path.clone(),
                name: m.name,
                ast,
                source: m.source,
            });
        }
    }

    // What the package asks of a `meadow` that is not stable: `@!feature(…)`
    // at the top of its root module, as `#![feature(…)]` is a crate's to say.
    let mut asked: Vec<String> = Vec::new();
    for m in &modules {
        for attr in &m.ast.value().attrs {
            let name = &**attr.name.value();
            let mut problem = |msg: String, span: Span| {
                diags.push(Diagnostic {
                    msg,
                    filename: m.source.name().to_string(),
                    label: ("written here".to_string(), span),
                    extra_labels: vec![],
                })
            };
            if name != "feature" {
                problem(
                    format!(
                        "`@!{name}` is not an attribute a module has: \
                         `@!feature(…)` is the one there is"
                    ),
                    attr.name.span,
                );
            } else if !m.path.is_empty() {
                problem(
                    "`@!feature(…)` is the package's to say: it goes at the top of \
                     the root module"
                        .to_string(),
                    attr.name.span,
                );
            } else {
                for meta in &attr.meta {
                    let feature = meta.name();
                    match crate::channel::feature_problem(
                        feature.value(),
                        crate::channel::accepting(),
                    ) {
                        Some(msg) => problem(msg, feature.span),
                        None => asked.push(feature.value().to_string()),
                    }
                }
            }
        }
    }

    // `Std.Ffi` is unstable: the package that names it has to have asked.
    for m in &modules {
        if let Some(span) = uses_ffi(&m.ast.value().decls)
            && let Some(msg) = crate::channel::ffi_problem(&asked)
        {
            diags.push(Diagnostic {
                msg,
                filename: m.source.name().to_string(),
                label: ("used here".to_string(), span),
                extra_labels: vec![],
            });
        }
    }

    let (mut cp, unit_diags) = meadow_compiler::compile_unit_with_procs(
        pkg.name, ident, pkg.name, pkg.id, modules, deps, opts, floor, procs,
    );
    // A package: a dependent names its values with `use`.
    cp.package = true;
    diags.extend(unit_diags);
    (cp, diags)
}

/// Like [`compile_str`], but with the embedded `Std` package as a dependency (so
/// the prelude is in scope) and everything linked into one runnable
/// [`core::Program`]. For tests / experiments that want the standard library.
pub fn compile_str_with_std(
    name: &str,
    src: &str,
    opts: Options,
) -> (core::Program, Vec<Diagnostic>) {
    let name = InternedString::from(name);
    let source = Source::new(SourceKind::Interactive, InternedString::from(src));
    let lex = tokenize(source);
    let (std_pkgs, mut diags) = stdlib::std_packages(opts);
    diags.extend(lex.errors.clone());
    let (ast, perrs) = parser::parse(name, source, &lex.tokens);
    for e in &perrs {
        diags.push(rich_to_diag(&source, e));
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
    let deps: Vec<meadow_compiler::Dep<'_>> =
        std_pkgs.iter().map(meadow_compiler::Dep::new).collect();
    let (cp, unit_diags) = compile_unit(name, std_pkgs.len(), modules, &deps, opts);
    diags.extend(unit_diags);

    let linked = Linker::link(std_pkgs.into_iter().chain(std::iter::once(cp)).collect());
    (linked.program, diags)
}

fn rich_to_diag(src: &Source, e: &Rich<'_, Token, Span>) -> Diagnostic {
    meadow_compiler::diagnostics::from_parse_error(&src.name().to_string(), e)
}
