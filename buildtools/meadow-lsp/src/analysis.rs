//! Compiling one open document, and indexing the result by source position.
//!
//! The compiler is built around whole packages, not editors: it hands back a
//! `CompiledPackage` with a `TypeTable` keyed by `NodeId` and an HIR tree whose
//! nodes carry spans. Everything an editor asks — *what is the type here*, *where
//! is this defined* — is a lookup by position, so this module walks the tree once
//! and builds the indices the requests need.
//!
//! `Std` is compiled once, before the server answers anything, and reused for
//! the life of the process. Not because it is slow — the whole library is a few
//! tens of milliseconds — but because a keystroke recompiles the open document
//! *only*, against that already-compiled library, and that is the whole reason
//! an edit costs a millisecond or two rather than the cost of the world.
//!
//! Note what this is not: there is no incremental compilation here, and none in
//! the batch compiler either. The unit is small and the dependency is memoised;
//! nothing is reused between one edit and the next.

use meadow_compiler::{
    AstModule, CompiledPackage, Options,
    diagnostics::Diagnostic,
    hir::{self, VarId},
    infer::TypeTable,
    intern::InternedString,
    lexer::tokenize,
    parser,
    source::{Source, SourceKind},
    span::Span,
};

/// Where something is defined: which source, and where in it.
///
/// A [`Span`] is a pair of byte offsets and nothing more, so it only means
/// something paired with the text it indexes. Within one document that pairing
/// is implicit; across modules it has to be written down.
#[derive(Debug, Clone, Copy)]
pub struct Loc {
    pub source: Source,
    pub span: Span,
}

/// Every binding in scope, and where it was written.
///
/// This works across modules for one reason, and it is worth stating because it
/// is the whole trick: the resolver brings a dependency's exported binding into
/// scope **under that dependency's own `VarId`** (`meadow_rename::Resolver::import`).
/// So a mention of `concatAll` in someone's file already carries the id that
/// `Std.String` gave it. Nothing has to be re-resolved to follow it — the lookup
/// is the same one it always was, and only the *map* has to be bigger than a
/// single file.
pub type DefIndex = std::collections::HashMap<VarId, Loc>;

/// Types and data constructors, which have no id to be keyed by.
///
/// A `VarId` is minted by the resolver for every *value* binding. A type
/// constructor never gets one — [`hir::TypeExpr::Con`] keeps the name and
/// checks it against the tycon environment — and neither does a data
/// constructor, so both are matched the only way the compiler matches them
/// anywhere: by name.
///
/// That is not a shortcut taken here. `CompiledPackage::ctor_fields` is keyed
/// by bare constructor name, and so is the tag table the back end builds, so
/// two packages declaring the same constructor already collide long before an
/// editor looks at them. Keying this by name is exactly as correct as the rest
/// of the compiler, and no more. Giving type and data constructors real ids
/// would fix all three together; it is a change to the resolver, not to this.
///
/// Types and constructors are separate maps because one name can be both:
/// `data Pair = Pair Int Int` declares a type `Pair` and a constructor `Pair`,
/// and a reference knows which it meant from where it appears.
#[derive(Default)]
pub struct NameIndex {
    pub types: std::collections::HashMap<InternedString, Loc>,
    pub ctors: std::collections::HashMap<InternedString, Loc>,
}

impl NameIndex {
    fn get(&self, name: InternedString, ns: Namespace) -> Option<Loc> {
        match ns {
            Namespace::Type => self.types.get(&name),
            Namespace::Ctor => self.ctors.get(&name),
        }
        .copied()
    }

    fn absorb(&mut self, other: NameIndex) {
        self.types.extend(other.types);
        self.ctors.extend(other.ctors);
    }
}

/// One inlay hint: where it goes, and what it says.
///
/// The label is a *sequence* rather than a string because the type names in it
/// are meant to be clicked. Each is resolved to a location when the request is
/// answered, not here, because the wider index lives with the standard library
/// rather than with one document.
#[derive(Debug, Clone)]
pub struct Hint {
    /// Byte offset the hint is inserted at.
    pub offset: u32,
    pub parts: Vec<HintPart>,
}

#[derive(Debug, Clone)]
pub enum HintPart {
    /// Punctuation and spacing: `(`, ` : `, `)`.
    Text(String),
    /// A type or effect constructor — something to link.
    Name(InternedString),
}

/// Split a rendered type into clickable parts.
///
/// Deliberately a scan of the rendered text rather than a second renderer.
/// `meadow_infer::write_type` handles functions, effect rows, records and
/// precedence, and a copy of it here would drift the first time either changed.
/// The scan is sound because of a property of the syntax it produces: a
/// capitalised identifier in a rendered type is a type or effect constructor,
/// and a variable is always lowercase (the `Namer` hands out `a`, `b`, …). So
/// the capitalised runs are exactly the names worth linking, wherever they
/// appear and however they nest.
fn hint_parts(rendered: &str) -> Vec<HintPart> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut ident = String::new();

    let flush_ident = |ident: &mut String, text: &mut String, parts: &mut Vec<HintPart>| {
        if ident.is_empty() {
            return;
        }
        if ident.starts_with(|c: char| c.is_ascii_uppercase()) {
            if !text.is_empty() {
                parts.push(HintPart::Text(std::mem::take(text)));
            }
            parts.push(HintPart::Name(InternedString::from(ident.as_str())));
        } else {
            text.push_str(ident);
        }
        ident.clear();
    };

    for c in rendered.chars() {
        if c.is_alphanumeric() || c == '_' || c == '\'' {
            ident.push(c);
        } else {
            flush_ident(&mut ident, &mut text, &mut parts);
            text.push(c);
        }
    }
    flush_ident(&mut ident, &mut text, &mut parts);
    if !text.is_empty() {
        parts.push(HintPart::Text(text));
    }
    parts
}

/// Which of the two capitalised namespaces a name was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// In a type position: `data Node a = Leaf (Vector a)`, or an effect label.
    Type,
    /// In an expression or a pattern: `Just x`.
    Ctor,
}

/// What a rename is renaming, and where it was declared.
enum Target {
    /// A value binding, matched by the id every reference to it carries.
    Value(VarId, (Option<Source>, Span)),
    /// A type or a data constructor, matched by name within its namespace.
    Name(InternedString, Namespace, (Option<Source>, Span)),
}

impl Target {
    /// Meadow's one spelling rule: a constructor and a type start with a
    /// capital, a value does not. Renaming across that line would not compile,
    /// so it is refused with the reason rather than attempted.
    fn check_new_name(&self, new: &str) -> Result<(), String> {
        let mut chars = new.chars();
        let first = chars
            .next()
            .ok_or_else(|| "a name cannot be empty".to_string())?;
        if !(first.is_alphabetic() || first == '_')
            || !chars.all(|c| c.is_alphanumeric() || c == '_' || c == '\'')
        {
            return Err(format!("`{new}` is not a name"));
        }
        let wants_upper = matches!(self, Target::Name(..));
        if wants_upper && !first.is_uppercase() {
            return Err(format!("`{new}` would have to start with a capital"));
        }
        if !wants_upper && first.is_uppercase() {
            return Err(format!("`{new}` would read as a constructor"));
        }
        Ok(())
    }
}

/// The message for a rename of something declared where this server cannot
/// edit: the standard library, or a dependency compiled from elsewhere.
fn outside(known: bool) -> String {
    if known {
        "this is defined outside the package, so renaming it here would not \
         match its definition"
            .to_string()
    } else {
        "nothing to rename here".to_string()
    }
}

/// Everything a completion could name, nearest first: this document's own
/// bindings, then the rest of the package, then its dependencies, then the
/// standard library.
///
/// A name is worth offering only if something can be said about it, so this
/// takes the bindings that have a scheme -- which is every top-level one.
fn gather_candidates(a: &mut Analysis, pkg: &CompiledPackage, deps: &[&CompiledPackage]) {
    let mut add =
        |name: String, scheme: meadow_compiler::infer::Scheme, distance: u8, from: String| {
            if name.starts_with(|c: char| c.is_uppercase()) {
                return;
            }
            let common = corpus().and_then(|c| c.get(&name).copied()).unwrap_or(0);
            a.candidates.push(Candidate {
                name,
                scheme,
                distance,
                from,
                common,
            });
        };
    for e in &pkg.exports {
        let module = if e.module.is_empty() {
            "this package".to_string()
        } else {
            e.module
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
                .join(".")
        };
        add(e.name.to_string(), e.scheme.clone(), 0, module);
    }
    for dep in deps {
        // The standard library is everywhere, so it sits behind anything the
        // project itself brought in.
        let distance = if dep.name.to_string().starts_with("Std") {
            3
        } else {
            2
        };
        let from = dep.name.to_string();
        for e in &dep.exports {
            // Only what can be written here without a qualifier. A name that
            // needs a `use` first would be offered as something that does not
            // compile, which is worse than not offering it.
            if let Some(flat) = &dep.prelude_exports
                && !flat.contains(&e.name)
            {
                continue;
            }
            add(e.name.to_string(), e.scheme.clone(), distance, from.clone());
        }
    }
    // The same function reached twice is one candidate; two *different*
    // functions of one name are two, and which of them fits is a question for
    // the type rather than for this. `map` is both `Vector`'s and `Maybe`'s,
    // and dropping either would take a real answer out of the list.
    a.candidates.sort_by(|x, y| {
        x.name
            .cmp(&y.name)
            .then_with(|| x.distance.cmp(&y.distance))
            .then_with(|| x.scheme.to_string().cmp(&y.scheme.to_string()))
    });
    a.candidates
        .dedup_by(|x, y| x.name == y.name && x.scheme.to_string() == y.scheme.to_string());
}

/// A value a completion could offer: what it is called, what it is, and where
/// it came from.
#[derive(Clone)]
pub struct Candidate {
    pub name: String,
    pub scheme: meadow_compiler::infer::Scheme,
    /// How near it is: 0 for this document's own bindings, 1 for the rest of
    /// this package, 2 for a dependency, 3 for the standard library.
    pub distance: u8,
    /// The package or module it came from, for the editor's detail line.
    pub from: String,
    /// How often the standard library's own source writes this name.
    ///
    /// A corpus of idiomatic Meadow that every install has, counted rather
    /// than curated: `map` and `foldl` are written constantly there and
    /// `splitAt` hardly ever, which is what a menu should reflect when it has
    /// nothing else to go on.
    pub common: u32,
}

/// How often each name appears in the standard library's source, counted once.
static CORPUS: std::sync::OnceLock<std::collections::HashMap<String, u32>> =
    std::sync::OnceLock::new();

fn corpus_from(sources: &[(&str, &str)]) -> &'static std::collections::HashMap<String, u32> {
    CORPUS.get_or_init(|| {
        let mut out: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for (_, source) in sources {
            let mut word = String::new();
            for c in source.chars() {
                if c.is_alphanumeric() || c == '_' {
                    word.push(c);
                } else if !word.is_empty() {
                    *out.entry(std::mem::take(&mut word)).or_default() += 1;
                }
            }
            if !word.is_empty() {
                *out.entry(std::mem::take(&mut word)).or_default() += 1;
            }
        }
        out
    })
}

/// What was counted, or nothing when the library's source never arrived.
fn corpus() -> Option<&'static std::collections::HashMap<String, u32>> {
    CORPUS.get()
}

/// Everything the server knows about one document.
pub struct Analysis {
    pub source: String,
    pub diagnostics: Vec<Diagnostic>,
    /// Every typed node, innermost last — so a position lookup can take the
    /// *smallest* span that contains it by scanning for the tightest match.
    pub typed: Vec<(Span, String)>,
    /// The same nodes, with the type itself rather than its rendering: what
    /// completion unifies against to find what a value can be piped into.
    pub typed_nodes: Vec<(Span, meadow_compiler::infer::Type)>,
    /// Every value a completion could name, with what it is.
    pub candidates: Vec<Candidate>,
    /// Where each binding *in this document* is introduced. Local on purpose:
    /// [`Analysis::var_at`] matches these spans against this document's offsets,
    /// so a definition in another file has no business here. The wider index is
    /// [`Std::definitions`].
    pub defs: DefIndex,
    /// Every mention of a binding, for go-to-definition.
    pub refs: Vec<(Span, VarId)>,
    /// Every mention of a type or a data constructor, and which of the two it
    /// was -- the namespaces are separate, so the reference has to say.
    pub name_refs: Vec<(Span, InternedString, Namespace)>,
    /// Types and constructors declared *in this document*, for the same reason
    /// [`Analysis::defs`] is local.
    pub names: NameIndex,
    /// Inlay hints: parameters and `let`s, which carry no annotation in the
    /// source. Written to read as what you could have typed -- `(x : Int)` --
    /// because the annotation syntax is real and someone may want to keep it.
    pub binders: Vec<Hint>,
    /// Names of top-level bindings, for hover headers.
    pub binding_names: std::collections::HashMap<VarId, String>,
    /// Rendered scheme of each top-level binding.
    pub schemes: std::collections::HashMap<VarId, String>,
    /// Type names and constructor names, so highlighting can tell `Maybe` from
    /// `Just` — both are capitalised, and only resolution knows which is which.
    pub types_in_scope: std::collections::HashSet<String>,
    /// Definitions in the *other* modules of this document's package, when it
    /// was analysed as part of one. Consulted after the document's own and
    /// before the standard library's, which is the order the resolver saw them
    /// in: a sibling's name arrives by `use`, and a local binding shadows it.
    pub wider: DefIndex,
    /// The same for the package's types and data constructors.
    pub wider_names: NameIndex,
    /// Every mention of a binding in the *other* modules of the package, with
    /// the file it is in. Only a rename needs these: go-to-definition asks
    /// where a name is declared, and a rename has to find everywhere it is
    /// written.
    pub wider_refs: Vec<(Source, Span, VarId)>,
    /// The same for types and data constructors.
    pub wider_name_refs: Vec<(Source, Span, InternedString, Namespace)>,
    /// The throwaway [`Source`] this document was compiled as. A definition
    /// whose `Loc` names a different one is in another file.
    pub source_id: meadow_compiler::source::SourceId,
    pub ctors_in_scope: std::collections::HashSet<String>,
    /// This document's top-level definitions, in source order -- what an
    /// editor offers to run or debug on its own.
    pub functions: Vec<Function>,
    /// Every data constructor this document could mention, by canonical name
    /// (`Tv.Link`), with the full path a `use` would spell it by:
    /// `demo.Syntax.Tv.Link`, `Std.Maybe.Maybe.Just`. What a hover shows.
    pub ctor_paths: std::collections::HashMap<InternedString, String>,
    /// What every dotted path reaches, for completing one.
    pub paths: PathIndex,
    /// Everything this document's package declares -- privately too, since
    /// they are the package's author's to find -- and what the package
    /// depends on declares publicly. What a workspace symbol search looks
    /// through, besides the library: see `meadow_find`.
    pub declarations: Vec<meadow_find::Decl>,
    /// What this document's definitions are lowered from, to show one in
    /// the compiler's IRs: see [`crate::ir`].
    pub ir: crate::ir::IrSource,
}

/// A top-level definition, as something to start a program at.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    /// The binding, so a caller can ask the package about it.
    pub var: VarId,
    /// Where its name is written.
    pub span: Span,
    /// `Some(qualified name)` when it is a `@test` -- the name `meadow test
    /// --exact` takes. `None` for anything else, and inside a `Std` module.
    pub test: Option<String>,
    /// How many parameters it is written with: `0` for a `def`.
    pub params: usize,
    /// Its inferred type, as a hover would show it.
    pub signature: String,
}

/// The files of one package on disk, as the editor should analyse them.
///
/// Built by whoever knows how packages are laid out — `meadow`, which owns
/// manifest parsing and module discovery and cannot be depended on from here
/// (it depends on this crate for its `lsp` subcommand). The server asks for one
/// through [`PackageLoader`] and does not care how it was found.
pub struct PackageSources {
    /// The package's name, which is also the first segment of a `use` path
    /// naming one of its own modules.
    pub name: InternedString,
    pub modules: Vec<ModuleFile>,
    /// What it depends on, compiled, each under the name this package calls
    /// it -- which is what its `use` lines say and need not be what the
    /// dependency calls itself.
    ///
    /// Compiled by whoever loaded the package, because building one is not
    /// something this crate knows how to do. Without them every name a
    /// dependency provides is undefined, which is what an editor used to show.
    pub deps: Vec<(InternedString, std::rc::Rc<CompiledPackage>)>,
    /// What can run a procedural macro one of them exports. `None` where
    /// nothing can, and then a call is reported rather than quietly ignored.
    pub procs: Option<std::rc::Rc<dyn meadow_compiler::expand::proc::Runner>>,
}

/// One `.mw` file of a package.
pub struct ModuleFile {
    /// Dotted path from the source root; empty for the root module.
    pub path: Vec<InternedString>,
    pub name: InternedString,
    pub file: std::path::PathBuf,
    pub text: String,
}

/// How the server finds the package a document belongs to: given any file
/// inside it, the whole thing; `None` if the file is not in a package at all;
/// or why a package it *is* in cannot be loaded -- a misnamed module file, a
/// dependency that is not there. That last case used to be `None` too, and the
/// document was silently analysed on its own, where every `use` of a sibling
/// failed and nothing said the real reason.
pub type PackageLoader = fn(&std::path::Path) -> Option<Result<PackageSources, String>>;

/// The standard library, compiled once.
pub struct Std {
    packages: Vec<CompiledPackage>,
    /// Each `Std` module as its own compiled unit, in dependency order, with the
    /// dotted name it is known by. Only needed to analyse a file that *is* one.
    modules: Vec<(String, CompiledPackage)>,
    types: std::collections::HashSet<String>,
    ctors: std::collections::HashSet<String>,
    /// Every binding the library defines, and where. Built once with the
    /// library, because it is the same walk over the same trees and the answer
    /// never changes.
    defs: DefIndex,
    /// The same for its types and data constructors.
    names: NameIndex,
    /// Where the embedded sources were written out, so a definition inside
    /// `Std` has a file an editor can open. `None` when nobody arranged it —
    /// go-to-definition then stops at the edge of the open document, which is
    /// what it did before this existed.
    src_root: Option<std::path::PathBuf>,
}

/// A `Std` module's path within the package.
///
/// `Lib` and `Prelude` sit at the root — they are what a dependent gets
/// unqualified — and everything else is nested by its dotted name. It mirrors
/// `meadow::stdlib::module_path`, which this crate cannot call: `meadow` depends
/// on it for the `lsp` subcommand, so the arrow only points one way. The test
/// that analyses every module keeps the two honest.
fn module_path(dotted: &str) -> Vec<InternedString> {
    if dotted == "Prelude" || dotted == "Lib" {
        Vec::new()
    } else {
        dotted.split('.').map(InternedString::from).collect()
    }
}

impl Std {
    /// Takes an already-compiled standard library. It is not loaded here on
    /// purpose: `meadow` owns the embedded sources and depends on this crate for
    /// its `lsp` subcommand, so reaching back for them would be a cycle.
    ///
    /// `modules` is the same library before bundling — see [`Std::module_at`].
    ///
    /// `src_root` is a directory holding the library's sources laid out by the
    /// relative names their [`SourceKind::File`] labels carry, so that a
    /// definition inside `Std` can be handed to an editor as a real file.
    pub fn new(
        packages: Vec<CompiledPackage>,
        modules: Vec<(String, CompiledPackage)>,
        src_root: Option<std::path::PathBuf>,
        // The library's own source, which completion counts names in: what
        // idiomatic Meadow reaches for, measured rather than curated. It lives
        // in the crate that embeds the library, which is the one that starts
        // this, so it arrives from there.
        sources: &[(&str, &str)],
    ) -> Std {
        let _ = corpus_from(sources);
        let mut types = std::collections::HashSet::new();
        let mut ctors = std::collections::HashSet::new();
        for p in &packages {
            collect_names(&p.data_decls, &mut types, &mut ctors);
        }
        // The bundle carries every module of the library, and it is the *same*
        // compile the pieces came from — `stdlib::std_packages` bundles what
        // `std_modules` produced — so one pass over it indexes both, and the
        // ids match either way a document was analysed.
        let mut defs = DefIndex::new();
        let mut names = NameIndex::default();
        for p in &packages {
            index_package(p, &mut defs, &mut names);
        }
        Std {
            packages,
            modules,
            types,
            ctors,
            defs,
            names,
            src_root,
        }
    }

    /// Every binding the standard library defines — the fallback when a name is
    /// in scope but was not written in the open document.
    /// The library's packages, compiled: what a program is lowered with.
    pub fn packages(&self) -> &[CompiledPackage] {
        &self.packages
    }

    pub fn definitions(&self) -> &DefIndex {
        &self.defs
    }

    /// Every type and data constructor the library declares, and where.
    pub fn declared_names(&self) -> &NameIndex {
        &self.names
    }

    /// A `file://` path for `source`, if one can be named.
    ///
    /// Modules discovered on disk carry an absolute path and need nothing.
    /// `Std`'s carry a *relative* label — `Std/Collections/Vector.mw` — because
    /// they were compiled from text baked into the binary, so they are resolved
    /// against wherever those sources were written out.
    pub fn path_of(&self, source: Source) -> Option<std::path::PathBuf> {
        let SourceKind::File(name) = source.kind else {
            return None;
        };
        let p = std::path::Path::new(&*name);
        if p.is_absolute() {
            return Some(p.to_path_buf());
        }
        let root = self.src_root.as_ref()?;
        let full = root.join(p);
        full.is_file().then_some(full)
    }

    /// Which `Std` module a document is, if it is one.
    ///
    /// `.../lib/Std/src/Collections/Vector.mw` is `Collections.Vector`. The name
    /// has to match one we were given, so a file that merely sits at a similar
    /// path in someone else's project is not mistaken for one.
    ///
    /// This matters because a `Std` module analysed the ordinary way — as a
    /// package depending on `Std` — declares every one of its own types a second
    /// time, and the editor fills with `already defined`.
    pub fn module_at(&self, uri: &str) -> Option<usize> {
        let path = uri.replace('\\', "/");
        let tail = path.rsplit_once("/Std/src/")?.1;
        let dotted = tail.strip_suffix(".mw")?.replace('/', ".");
        self.modules.iter().position(|(name, _)| *name == dotted)
    }

    /// The dotted name of the module at `index`, for reporting.
    pub fn module_name(&self, index: usize) -> &str {
        &self.modules[index].0
    }

    /// Compile `text` as a throwaway one-module package against `Std`.
    pub fn analyse(&self, text: &str) -> Analysis {
        let deps: Vec<&CompiledPackage> = self.packages.iter().collect();
        let name = InternedString::from("main");
        self.compile(text, name, name, Vec::new(), 1, &deps, None)
    }

    /// Compile `text` as the `Std` module it is: in its own place in the
    /// package, against the modules declared before it and nothing after.
    pub fn analyse_module(&self, index: usize, text: &str) -> Analysis {
        let (dotted, _) = &self.modules[index];
        let path = module_path(dotted);
        let unit = InternedString::from(dotted.as_str());
        let name = path.last().copied().unwrap_or(unit);
        let deps: Vec<&CompiledPackage> = self.modules[..index].iter().map(|(_, p)| p).collect();
        // Compiled *inside* the package, not merely against it. `Bool` says
        // `use Std.Test (assertEq)`, and a unit that does not know it belongs to
        // `Std` cannot resolve that.
        //
        // The package name comes off the bundle, whose `name` is the package;
        // a sub-unit's is its own unit name, which would make `Std.Test` look
        // like `Test.Test`.
        let package = self.packages.first().map(|p| p.name);
        self.compile(text, name, unit, path, index, &deps, package)
    }

    /// Compile `open`'s package as one unit, with `text` standing in for the
    /// file on disk.
    ///
    /// This is what makes a multi-module package work in an editor. Analysed on
    /// its own a module cannot see its siblings — the package is the
    /// compilation unit, and each module is a namespace inside it, so a `use`
    /// of a sibling has nothing to resolve against and every name it brought in
    /// is reported undefined. Compiling the package puts them back.
    ///
    /// Only the open document's spans are recorded in full: the rest of the
    /// package is indexed for go-to-definition and nothing else, and its
    /// diagnostics belong to files the editor did not ask about.
    pub fn analyse_package(
        &self,
        sources: &PackageSources,
        open: &std::path::Path,
        text: &str,
    ) -> Option<Analysis> {
        let deps: Vec<&CompiledPackage> = self.packages.iter().collect();
        let mut here: Option<Source> = None;
        let mut modules: Vec<AstModule> = Vec::new();
        let mut diagnostics: Vec<Diagnostic> = Vec::new();
        let mut calls: Vec<Span> = Vec::new();

        for m in &sources.modules {
            let mine = m.file == open;
            // The open buffer is newer than the file it came from, and it is
            // the one the editor is asking about.
            let body = if mine { text } else { &m.text };
            let name = InternedString::from(m.file.display().to_string());
            let source = Source::new(SourceKind::File(name), body.into());
            if mine {
                here = Some(source);
            }
            let lex = tokenize(source);
            let (ast, perrs) = parser::parse(m.name, source, &lex.tokens);
            if mine {
                diagnostics.extend(lex.errors);
                for e in &perrs {
                    diagnostics.push(meadow_compiler::diagnostics::from_parse_error(
                        &name.to_string(),
                        e,
                    ));
                }
            }
            if let Some(ast) = ast {
                if mine {
                    calls = macro_calls(&ast);
                }
                modules.push(AstModule {
                    path: m.path.clone(),
                    name: m.name,
                    ast,
                    source,
                });
            }
        }
        let here = here?;

        // The standard library, and then what this package itself depends on,
        // each under the name it calls it by.
        let mut views: Vec<meadow_compiler::Dep<'_>> = deps
            .iter()
            .copied()
            .map(meadow_compiler::Dep::new)
            .collect();
        views.extend(
            sources
                .deps
                .iter()
                .map(|(alias, pkg)| meadow_compiler::Dep::named(*alias, pkg.as_ref())),
        );
        let (pkg, unit_diags) = match &sources.procs {
            Some(procs) => meadow_compiler::compile_unit_with_procs(
                sources.name,
                sources.name,
                sources.name,
                1,
                modules,
                &views,
                Options::debug(),
                0,
                procs.as_ref(),
            ),
            None => meadow_compiler::compile_unit_in_package(
                sources.name,
                sources.name,
                1,
                modules,
                &views,
                Options::debug(),
            ),
        };
        // A diagnostic names the file it is in, so the ones for the rest of the
        // package are simply not this document's to report.
        let mine = here.name().to_string();
        diagnostics.extend(unit_diags.into_iter().filter(|d| d.filename == mine));

        let mut a = Analysis {
            source: text.to_string(),
            source_id: here.id,
            diagnostics,
            typed: Vec::new(),
            typed_nodes: Vec::new(),
            candidates: Vec::new(),
            defs: Default::default(),
            refs: Vec::new(),
            binders: Vec::new(),
            binding_names: Default::default(),
            schemes: Default::default(),
            name_refs: Vec::new(),
            names: Default::default(),
            types_in_scope: self.types.clone(),
            ctors_in_scope: self.ctors.clone(),
            wider: Default::default(),
            wider_names: Default::default(),
            wider_refs: Vec::new(),
            wider_name_refs: Vec::new(),
            functions: Vec::new(),
            ctor_paths: Default::default(),
            paths: Default::default(),
            declarations: Vec::new(),
            ir: Default::default(),
        };
        collect_names(
            &pkg.data_decls,
            &mut a.types_in_scope,
            &mut a.ctors_in_scope,
        );
        a.declarations =
            meadow_find::collect::package(&pkg, meadow_find::collect::Options { private: true });
        for (_, dep) in &sources.deps {
            a.declarations
                .extend(meadow_find::collect::package(dep, Default::default()));
        }
        a.ir = crate::ir::IrSource::of(
            &pkg,
            deps.iter()
                .copied()
                .chain(sources.deps.iter().map(|(_, p)| p.as_ref())),
            sources.deps.iter().map(|(_, p)| p.clone()).collect(),
        );
        for e in &pkg.exports {
            a.binding_names.insert(e.var, e.name.to_string());
            a.schemes.insert(e.var, e.scheme.to_string());
        }
        gather_candidates(&mut a, &pkg, &deps);
        // Every other binding that was generalized -- private top-level ones,
        // and local `let`s -- hovers as its scheme too, rather than as the type
        // its binder node happened to have. See `CompiledPackage::generalized`
        // for why the difference shows: a local `let rec` read `! c`.
        for (var, scheme) in &pkg.generalized {
            a.schemes.entry(*var).or_insert_with(|| scheme.to_string());
        }
        for m in &pkg.modules {
            if m.source.id == here.id {
                let mut w = Walk {
                    types: Some(&pkg.types),
                    source: m.source,
                    namer: meadow_compiler::infer::Renderer::for_table(&pkg.types),
                    a: &mut a,
                };
                w.module(&m.hir);
                absorb_refs(&mut a, m);
            } else {
                // A sibling: where its names are, and nothing else.
                let mut scratch = Analysis::empty();
                let mut w = Walk {
                    types: None,
                    source: m.source,
                    namer: meadow_compiler::infer::Renderer::new(),
                    a: &mut scratch,
                };
                w.module(&m.hir);
                absorb_refs(&mut scratch, m);
                a.wider.extend(scratch.defs);
                a.wider_names.absorb(scratch.names);
                a.wider_refs
                    .extend(scratch.refs.into_iter().map(|(s, v)| (m.source, s, v)));
                a.wider_name_refs.extend(
                    scratch
                        .name_refs
                        .into_iter()
                        .map(|(s, n, ns)| (m.source, s, n, ns)),
                );
            }
        }
        mark_tests(&mut a, &pkg);
        a.ctor_paths = ctor_paths(&pkg, Some(sources.name), &deps, None);
        a.paths = path_index(&pkg, Some(sources.name), &deps, None);
        a.leave_out_expansions(&calls);
        Some(a)
    }

    #[allow(clippy::too_many_arguments)]
    fn compile(
        &self,
        text: &str,
        name: InternedString,
        unit: InternedString,
        path: Vec<InternedString>,
        id: usize,
        deps: &[&CompiledPackage],
        package: Option<InternedString>,
    ) -> Analysis {
        let source = Source::new(SourceKind::Interactive, text.into());
        let lex = tokenize(source);
        let mut diagnostics = lex.errors;
        let (ast, perrs) = parser::parse(name, source, &lex.tokens);
        for e in &perrs {
            diagnostics.push(meadow_compiler::diagnostics::from_parse_error("main", e));
        }
        let calls = ast.as_ref().map(macro_calls).unwrap_or_default();

        let modules = ast
            .map(|ast| {
                vec![AstModule {
                    path,
                    name,
                    ast,
                    source,
                }]
            })
            .unwrap_or_default();

        let views: Vec<meadow_compiler::Dep<'_>> = deps
            .iter()
            .copied()
            .map(meadow_compiler::Dep::new)
            .collect();
        let (pkg, mut unit_diags) = match package {
            Some(p) => meadow_compiler::compile_unit_in_package(
                p,
                unit,
                id,
                modules,
                &views,
                Options::debug(),
            ),
            None => meadow_compiler::compile_unit(name, id, modules, &views, Options::debug()),
        };
        diagnostics.append(&mut unit_diags);

        let mut a = Analysis {
            source: text.to_string(),
            source_id: source.id,
            diagnostics,
            typed: Vec::new(),
            typed_nodes: Vec::new(),
            candidates: Vec::new(),
            defs: Default::default(),
            refs: Vec::new(),
            binders: Vec::new(),
            binding_names: Default::default(),
            schemes: Default::default(),
            name_refs: Vec::new(),
            names: Default::default(),
            types_in_scope: self.types.clone(),
            ctors_in_scope: self.ctors.clone(),
            wider: Default::default(),
            wider_names: Default::default(),
            wider_refs: Vec::new(),
            wider_name_refs: Vec::new(),
            functions: Vec::new(),
            ctor_paths: Default::default(),
            paths: Default::default(),
            declarations: Vec::new(),
            ir: Default::default(),
        };
        collect_names(
            &pkg.data_decls,
            &mut a.types_in_scope,
            &mut a.ctors_in_scope,
        );
        a.declarations =
            meadow_find::collect::package(&pkg, meadow_find::collect::Options { private: true });
        for e in &pkg.exports {
            a.binding_names.insert(e.var, e.name.to_string());
            a.schemes.insert(e.var, e.scheme.to_string());
        }
        a.ir = crate::ir::IrSource::of(&pkg, deps.iter().copied(), Vec::new());
        gather_candidates(&mut a, &pkg, deps);
        // Every other binding that was generalized -- private top-level ones,
        // and local `let`s -- hovers as its scheme too, rather than as the type
        // its binder node happened to have. See `CompiledPackage::generalized`
        // for why the difference shows: a local `let rec` read `! c`.
        for (var, scheme) in &pkg.generalized {
            a.schemes.entry(*var).or_insert_with(|| scheme.to_string());
        }
        for m in &pkg.modules {
            let mut w = Walk {
                types: Some(&pkg.types),
                source: m.source,
                namer: meadow_compiler::infer::Renderer::for_table(&pkg.types),
                a: &mut a,
            };
            w.module(&m.hir);
            absorb_refs(&mut a, m);
        }
        // Not in a `Std` module. The standard library is compiled into the
        // binary, so `meadow test --std` runs that copy rather than this file --
        // a Test lens here would report on code other than what is on screen,
        // and after an edit could call a broken fix a pass.
        if package.is_none() {
            mark_tests(&mut a, &pkg);
        }
        // A throwaway single-file analysis is not a package anyone can name, so
        // its own constructors are shown from the module down.
        a.ctor_paths = ctor_paths(&pkg, package, deps, package);
        a.paths = path_index(&pkg, package, deps, package);
        a.leave_out_expansions(&calls);
        a
    }
}

/// What a unit already compiled says about one of its sources: what each
/// name in it resolved to and what type each node has. Only what is read off
/// the typed tree -- no candidates, no paths, no declarations -- so it is
/// cheap enough to ask after every key: the REPL does, of the entry being
/// typed, to colour it as an editor would.
pub fn of_unit(pkg: &CompiledPackage, text: &str, source: Source) -> Analysis {
    let mut a = Analysis {
        source: text.to_string(),
        source_id: source.id,
        diagnostics: Vec::new(),
        typed: Vec::new(),
        typed_nodes: Vec::new(),
        candidates: Vec::new(),
        defs: Default::default(),
        refs: Vec::new(),
        binders: Vec::new(),
        binding_names: Default::default(),
        schemes: Default::default(),
        name_refs: Vec::new(),
        names: Default::default(),
        types_in_scope: Default::default(),
        ctors_in_scope: Default::default(),
        wider: Default::default(),
        wider_names: Default::default(),
        wider_refs: Vec::new(),
        wider_name_refs: Vec::new(),
        functions: Vec::new(),
        ctor_paths: Default::default(),
        paths: Default::default(),
        declarations: Vec::new(),
        ir: Default::default(),
    };
    for m in &pkg.modules {
        if m.source.id != source.id {
            continue;
        }
        let mut w = Walk {
            types: Some(&pkg.types),
            source: m.source,
            namer: meadow_compiler::infer::Renderer::for_table(&pkg.types),
            a: &mut a,
        };
        w.module(&m.hir);
        absorb_refs(&mut a, m);
    }
    a
}

/// Where each macro call standing for declarations is: `syntax! { … }`,
/// `lang! { … }`, `derive! …`, attributed or not -- and each declaration
/// with a `@derive(…)` on it.
fn macro_calls(ast: &meadow_compiler::span::Located<meadow_compiler::ast::Module>) -> Vec<Span> {
    use meadow_compiler::ast::Decl;
    // A `@derive(…)` is a macro call too -- `@derive(Lexer)` writes a whole
    // lexer -- and what it writes has the declaration's spans. The `data` it
    // is on has no functions or parameters of its own to lose.
    fn call(d: &Decl) -> bool {
        match d {
            Decl::MacCall(_) => true,
            Decl::Attributed(attrs, inner) => {
                attrs.iter().any(|a| &**a.name.value() == "derive") || call(inner.value())
            }
            _ => false,
        }
    }
    ast.value()
        .decls
        .iter()
        .filter(|d| call(d.value()))
        .map(|d| d.span)
        .collect()
}

impl Analysis {
    /// Leave out what a macro call's expansion would otherwise put on it.
    ///
    /// What a macro generates is declarations like any other, and its spans
    /// are the call's: every function it makes got a Debug lens on the call,
    /// and every parameter a `(x : T)` hint inside it -- dozens, for a
    /// `syntax!`. A macro call is not the code it stands for. It keeps hover
    /// and go-to-definition, which are asked about a place, and its
    /// diagnostics; it loses the lenses and hints, which are put on places
    /// unasked.
    fn leave_out_expansions(&mut self, calls: &[Span]) {
        if calls.is_empty() {
            return;
        }
        // Up to the end, inclusive: a generated binder spans the whole call,
        // so the closing half of its hint -- ` : T)` -- is at the call's end.
        let inside = |at: u32| calls.iter().any(|c| c.start <= at && at <= c.end);
        self.functions.retain(|f| !inside(f.span.start));
        self.binders.retain(|h| !inside(h.offset));
    }
}

/// What a path segment names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathKind {
    Module,
    Type,
    /// A data constructor, which lives under the type that owns it.
    Ctor,
    Value,
}

/// One name that may follow a path.
#[derive(Clone, Debug)]
pub struct PathEntry {
    pub name: String,
    pub kind: PathKind,
    /// The type, for something that has one; for a constructor, the type it
    /// builds.
    pub detail: String,
}

/// What sits directly under each dotted path.
///
/// One index answers every question about a qualified name, because they are
/// all the same question asked at a different depth. `use Std.` wants the
/// modules under `Std`; `use Std.Maybe (` wants what that module exports; `M.`
/// in an expression wants the same thing, under whatever `M` was aliased to;
/// and `Shape.` wants a type's constructors -- a type being a path like any
/// other, with its constructors under it.
#[derive(Default, Clone)]
pub struct PathIndex {
    under: std::collections::HashMap<String, Vec<PathEntry>>,
}

impl PathIndex {
    /// The names directly under `path`. The empty path is the top: the
    /// packages, and whatever the open document declares for itself.
    pub fn under(&self, path: &str) -> &[PathEntry] {
        self.under.get(path).map(|v| &v[..]).unwrap_or(&[])
    }

    /// Every path whose last segments are `written`.
    ///
    /// A name is rarely written in full. A type is named bare in an expression
    /// -- `Maybe.Just`, never `Std.Maybe.Maybe.Just` -- and a `use` inside a
    /// package may leave the package off, so what was typed is matched as a
    /// suffix and the shortest match wins.
    pub fn ending_in(&self, written: &str) -> Vec<&str> {
        let want: Vec<&str> = written.split('.').collect();
        let mut out: Vec<&str> = self
            .under
            .keys()
            .filter(|k| {
                let segs: Vec<&str> = k.split('.').collect();
                segs.len() >= want.len() && segs[segs.len() - want.len()..] == want[..]
            })
            .map(|k| k.as_str())
            .collect();
        out.sort_by_key(|k| (k.len(), *k));
        out
    }

    fn add(&mut self, path: String, entry: PathEntry) {
        let under = self.under.entry(path).or_default();
        if !under
            .iter()
            .any(|e| e.name == entry.name && e.kind == entry.kind)
        {
            under.push(entry);
        }
    }

    /// Record one module: the path that reaches it, what it declares, and what
    /// it exports.
    fn module(
        &mut self,
        package: Option<InternedString>,
        m: &hir::LModule,
        path: &[InternedString],
        pkg: &CompiledPackage,
        // Whether `pkg` is this module compiled on its own, in which case
        // everything it exports is this module's rather than some other's.
        alone: bool,
    ) {
        let mut segs: Vec<String> = package.into_iter().map(|p| p.to_string()).collect();
        segs.extend(path.iter().map(|s| s.to_string()));
        // Every step of the way down, not just the last: a package whose
        // intermediate module holds nothing but `mod` declarations is still
        // something to offer on the way to what is under it.
        for i in 0..segs.len() {
            self.add(
                segs[..i].join("."),
                PathEntry {
                    name: segs[i].clone(),
                    kind: PathKind::Module,
                    detail: String::new(),
                },
            );
        }
        let here = segs.join(".");
        for d in &m.value().decls {
            let (name, ctors): (String, Vec<(String, String)>) = match d.value() {
                hir::Decl::Data(dd) => {
                    let ty = hir::spelling(&dd.name).to_string();
                    let ctors = dd
                        .variants
                        .iter()
                        .map(|v| (bare_ctor(hir::spelling(&v.name)), ty.clone()))
                        .collect();
                    (ty, ctors)
                }
                hir::Decl::Record(rd) => {
                    let ty = hir::spelling(&rd.name).to_string();
                    (ty.clone(), vec![(ty.clone(), ty)])
                }
                hir::Decl::Effect(ed) => (hir::spelling(&ed.name).to_string(), Vec::new()),
                hir::Decl::Alias(ad) => (hir::spelling(&ad.name).to_string(), Vec::new()),
                _ => continue,
            };
            self.add(
                here.clone(),
                PathEntry {
                    name: name.clone(),
                    kind: PathKind::Type,
                    detail: String::new(),
                },
            );
            let owner = join(&here, &name);
            for (ctor, of) in ctors {
                self.add(
                    owner.clone(),
                    PathEntry {
                        name: ctor,
                        kind: PathKind::Ctor,
                        detail: of,
                    },
                );
            }
        }
        for e in &pkg.exports {
            if alone || e.module.as_slice() == path {
                self.add(
                    here.clone(),
                    PathEntry {
                        name: e.name.to_string(),
                        kind: PathKind::Value,
                        detail: e.scheme.to_string(),
                    },
                );
            }
        }
    }
}

/// `a.b` from `a` and `b`, where an empty `a` is the top.
fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

/// A variant's name as the source writes it: the HIR's is canonical
/// (`Maybe.Just`), and what is typed after the dot is only its last segment.
fn bare_ctor(spelled: &str) -> String {
    spelled
        .rsplit_once('.')
        .map(|(_, c)| c.to_string())
        .unwrap_or_else(|| spelled.to_string())
}

/// Every path in `pkg` and its dependencies. The arguments are
/// [`ctor_paths`]'s, and mean the same thing.
fn path_index(
    pkg: &CompiledPackage,
    own: Option<InternedString>,
    deps: &[&CompiledPackage],
    sub_units_of: Option<InternedString>,
) -> PathIndex {
    let mut out = PathIndex::default();
    for dep in deps {
        for m in &dep.modules {
            let dotted: Vec<String> = m.path.iter().map(|s| s.to_string()).collect();
            let alone = sub_units_of.is_some() && dep.name.to_string() == dotted.join(".");
            let package = match sub_units_of {
                Some(p) if alone => p,
                _ => dep.name,
            };
            out.module(Some(package), &m.hir, &m.path, dep, alone);
        }
    }
    for m in &pkg.modules {
        out.module(own, &m.hir, &m.path, pkg, false);
    }
    out
}

/// The full path of every data constructor declared in `pkg` or its
/// dependencies: package, module, type, constructor.
///
/// `own` names `pkg`, or is `None` when it has no name worth showing. A `Std`
/// module analysed as itself depends on the modules before it, each a unit
/// named by its dotted path rather than by the package; `sub_units_of` is the
/// package those belong to.
fn ctor_paths(
    pkg: &CompiledPackage,
    own: Option<InternedString>,
    deps: &[&CompiledPackage],
    sub_units_of: Option<InternedString>,
) -> std::collections::HashMap<InternedString, String> {
    let mut out = std::collections::HashMap::new();
    let mut add =
        |package: Option<InternedString>, module: &hir::LModule, path: &[InternedString]| {
            let mut prefix: Vec<String> = package.into_iter().map(|p| p.to_string()).collect();
            prefix.extend(path.iter().map(|s| s.to_string()));
            for d in &module.value().decls {
                let ctors: Vec<InternedString> = match d.value() {
                    hir::Decl::Data(dd) => dd.variants.iter().map(|v| v.name).collect(),
                    hir::Decl::Record(rd) => vec![rd.ctor],
                    _ => continue,
                };
                for c in ctors {
                    let mut full = prefix.clone();
                    // The name carries the package past the resolver; the path
                    // already names it, and a hover spells it as the source does.
                    full.push(hir::ctor_spelling(&c.to_string()).to_string());
                    out.insert(c, full.join("."));
                }
            }
        };
    for dep in deps {
        for m in &dep.modules {
            let dotted: Vec<String> = m.path.iter().map(|s| s.to_string()).collect();
            let package = match sub_units_of {
                Some(p) if dep.name.to_string() == dotted.join(".") => p,
                _ => dep.name,
            };
            add(Some(package), &m.hir, &m.path);
        }
    }
    for m in &pkg.modules {
        add(own, &m.hir, &m.path);
    }
    out
}

/// Mark which of the analysed functions are `@test`s, under the name
/// `meadow test` knows each by.
///
/// The name is the qualified one from [`meadow_compiler::TestSite::qualified`],
/// which the runner prints and matches with `--exact`; spelling it anywhere
/// else would let the lens and the runner disagree about which test is which.
fn mark_tests(a: &mut Analysis, pkg: &CompiledPackage) {
    let sites: std::collections::HashMap<VarId, String> = pkg
        .test_sites()
        .into_iter()
        .map(|t| (t.var, t.qualified()))
        .collect();
    for f in &mut a.functions {
        f.test = sites.get(&f.var).cloned();
    }
}

/// `t` without a trailing `! e` whose variable is mentioned nowhere else.
///
/// A scheme prints without one (see `meadow_infer`'s rendering of `Scheme`),
/// because an effect variable that occurs once constrains nothing; a node's
/// type, which is all a definition that is not exported has, still carries it.
fn without_lone_effect(t: &str) -> String {
    if let Some((body, var)) = t.rsplit_once(" ! ") {
        // A variable is written small; `Eff` is an effect's name.
        let lone = var.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '\'')
            && !body
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '\''))
                .any(|w| w == var);
        if lone {
            return body.to_string();
        }
    }
    t.to_string()
}

/// Fold in the references the HIR does not carry: `use` lists, and the type
/// qualifying a `Type.Ctor`. Without these a rename would leave the name
/// behind in exactly the places that make a package stop compiling.
fn absorb_refs(a: &mut Analysis, m: &meadow_compiler::TypedModule) {
    for r in &m.refs {
        match r.what {
            meadow_compiler::rename::NameRef::Value(v) => a.refs.push((r.span, v)),
            meadow_compiler::rename::NameRef::Type(n) => {
                a.name_refs.push((r.span, n, Namespace::Type))
            }
            meadow_compiler::rename::NameRef::Ctor(n) => {
                a.name_refs.push((r.span, n, Namespace::Ctor))
            }
        }
    }
    // What a derive wrote stands at an empty span: it is nobody's text, and
    // nothing an editor points at should find it.
    a.refs.retain(|(s, _)| s.end > s.start);
    a.name_refs.retain(|(s, _, _)| s.end > s.start);
}

/// What a name written in a module names.
#[derive(Debug, Clone, Copy)]
pub enum Mentioned {
    Value(VarId),
    Type(InternedString),
    Ctor(InternedString),
}

/// A name written in a module: where, what it names, and where that is
/// declared -- `None` for what no module declares, a primitive.
#[derive(Debug, Clone, Copy)]
pub struct Mention {
    pub span: Span,
    pub what: Mentioned,
    pub declared: Option<Loc>,
}

/// Every name each module of `pkg` writes, in the order the walk meets them,
/// with where what it names is declared: in `pkg`, or in one of `others`, the
/// packages it was compiled against. The resolver's answers as an editor reads
/// them, and what another resolver -- MeadowBoot's -- is checked against.
pub fn mentions(pkg: &CompiledPackage, others: &[&CompiledPackage]) -> Vec<(Source, Vec<Mention>)> {
    let mut defs = DefIndex::default();
    let mut names = NameIndex::default();
    for p in others.iter().copied().chain([pkg]) {
        index_package(p, &mut defs, &mut names);
    }
    pkg.modules
        .iter()
        .map(|m| {
            let mut a = Analysis::empty();
            let mut w = Walk {
                types: None,
                source: m.source,
                namer: meadow_compiler::infer::Renderer::new(),
                a: &mut a,
            };
            w.module(&m.hir);
            absorb_refs(&mut a, m);
            let values = a.refs.iter().map(|&(span, v)| Mention {
                span,
                what: Mentioned::Value(v),
                declared: defs.get(&v).copied(),
            });
            let named = a.name_refs.iter().map(|&(span, n, ns)| Mention {
                span,
                what: match ns {
                    Namespace::Type => Mentioned::Type(n),
                    Namespace::Ctor => Mentioned::Ctor(n),
                },
                declared: names.get(n, ns),
            });
            (m.source, values.chain(named).collect())
        })
        .collect()
}

/// Walk every module of `pkg` for its definitions alone.
///
/// Cheaper than it looks next to the full walk: rendering a type to a string is
/// what costs, and a package nobody has open needs none of them — so `types` is
/// `None` and `record` does nothing.
fn index_package(pkg: &CompiledPackage, into: &mut DefIndex, names: &mut NameIndex) {
    for m in &pkg.modules {
        let mut scratch = Analysis::empty();
        let mut w = Walk {
            types: None,
            source: m.source,
            namer: meadow_compiler::infer::Renderer::new(),
            a: &mut scratch,
        };
        w.module(&m.hir);
        into.extend(scratch.defs);
        names.absorb(scratch.names);
    }
}

struct Walk<'a> {
    /// `None` while indexing a dependency: it suppresses every `to_string` on a
    /// type, which is the whole cost of the walk.
    types: Option<&'a TypeTable>,
    /// What the spans this walk records are offsets into.
    source: Source,
    /// Names type variables for the hints of the declaration being walked.
    /// Shared across it so that one letter means one variable -- see
    /// [`meadow_compiler::infer::Renderer`].
    namer: meadow_compiler::infer::Renderer,
    a: &'a mut Analysis,
}

impl Walk<'_> {
    fn record(&mut self, span: Span, id: hir::NodeId) {
        if let Some(ty) = self.types.and_then(|t| t.get(id)) {
            let rendered = ty.to_string();
            self.a.typed_nodes.push((span, ty.clone()));
            self.a.typed.push((span, rendered));
        }
    }

    /// The type at `id`, named consistently with the rest of this declaration.
    fn hint_type(&mut self, id: hir::NodeId) -> Option<String> {
        let ty = self.types.and_then(|t| t.get(id))?.clone();
        Some(self.namer.render(&ty))
    }

    /// The declared result of a function of `arity` parameters: what follows
    /// the last arrow, effect and all.
    ///
    /// Read off the *function's* type rather than the body's, because an
    /// effect belongs to the arrow. `fun logIt x = let _ = println "hi" in x`
    /// has a body of type `a` and a type of `a -> a ! { Console | e }`; hinting the
    /// body would claim the function is pure.
    fn result_hint(&mut self, fn_id: hir::NodeId, arity: usize) -> Option<String> {
        let whole = self.types.and_then(|t| t.get(fn_id))?.clone();
        let mut ty = whole.clone();
        let mut effect = meadow_compiler::infer::Type::RowEmpty;
        for _ in 0..arity {
            match ty {
                meadow_compiler::infer::Type::Fun(_, ret, eff) => {
                    effect = (*eff).clone();
                    ty = (*ret).clone();
                }
                // Fewer arrows than parameters: inference gave up somewhere.
                _ => return None,
            }
        }
        // An effect that is nothing but a row variable, mentioned nowhere
        // else, says nothing: *every* pure function has one, and the scheme
        // printer hides it for the same reason. It is worth showing exactly
        // when it is shared — `apply2 : (a -> a ! e) -> a -> a ! e` is telling
        // you the result carries whatever `f` does.
        if let meadow_compiler::infer::Type::Var(v) = effect {
            if occurrences(&whole, v) <= 1 {
                effect = meadow_compiler::infer::Type::RowEmpty;
            }
        }
        Some(self.namer.render_result(&ty, &effect))
    }

    /// A binder: remember where it was introduced, and its type if there is one
    /// worth showing.
    fn bind(&mut self, ident: &hir::Ident, id: hir::NodeId, hint: bool) {
        let var = *ident.value();
        self.a.defs.insert(
            var,
            Loc {
                source: self.source,
                span: ident.span,
            },
        );
        if let Some(ty) = self.types.and_then(|t| t.get(id)) {
            self.a.typed.push((ident.span, ty.to_string()));
        }
        if let Some(rendered) = hint.then(|| self.hint_type(id)).flatten() {
            self.a.annotate(ident.span, &rendered);
        }
    }

    fn module(&mut self, m: &hir::LModule) {
        for d in &m.value().decls {
            self.decl(d);
        }
    }

    fn decl(&mut self, d: &hir::LDecl) {
        match d.value() {
            hir::Decl::Bind(b) => {
                // A fresh naming per declaration: `a` in one function has nothing
                // to do with `a` in the next, and sharing the namer across a
                // whole module would give unrelated functions different
                // letters for no reason.
                self.namer = match self.types {
                    Some(types) => meadow_compiler::infer::Renderer::for_table(types),
                    None => meadow_compiler::infer::Renderer::new(),
                };
                self.function(b);
                self.bind_decl(b, false)
            }

            hir::Decl::Data(dd) => {
                self.declare(Namespace::Type, dd.name, dd.name_span);
                for v in &dd.variants {
                    self.declare(Namespace::Ctor, v.name, v.name_span);
                    match &v.fields {
                        hir::VariantFields::Positional(ts) => ts.iter().for_each(|t| self.ty(t)),
                        hir::VariantFields::Named(fs) => fs.iter().for_each(|(_, t)| self.ty(t)),
                    }
                }
            }
            hir::Decl::Record(rd) => {
                self.declare(Namespace::Type, rd.name, rd.name_span);
                rd.fields.iter().for_each(|(_, t)| self.ty(t));
            }
            hir::Decl::Effect(ed) => {
                self.declare(Namespace::Type, ed.name, ed.name_span);
                // An operation is callable as a value, so it has a `VarId` and
                // belongs in the ordinary definition index as well.
                for (_, ident, t) in &ed.ops {
                    self.bind(ident, ident.id, false);
                    self.ty(t);
                }
            }
            hir::Decl::Alias(ad) => {
                self.declare(Namespace::Type, ad.name, ad.name_span);
                self.ty(&ad.ty);
            }
            hir::Decl::EffectAlias(ad) => {
                self.declare(Namespace::Type, ad.name, ad.name_span);
                for (_, args) in &ad.row.labels {
                    args.iter().for_each(|a| self.ty(a));
                }
            }
            // The name in a signature is a use of the binding it describes.
            hir::Decl::Sig(ident, t, bounds) => {
                self.a.refs.push((ident.span, *ident.value()));
                self.ty(t);
                bounds.iter().for_each(|b| self.bound(b));
            }
            hir::Decl::Trait(td) => {
                self.declare(Namespace::Type, td.name, td.name_span);
                for (s, _) in &td.supers {
                    self.a.name_refs.push((s.span, *s.value(), Namespace::Type));
                }
                for a in &td.assocs {
                    self.declare(Namespace::Type, *a.value(), a.span);
                }
                for m in &td.methods {
                    // A method is called as a value, so it is defined as one.
                    self.bind(&m.var, m.var.id, false);
                    self.ty(&m.ty);
                    if let Some(hir::DefaultMethod {
                        body: Some(body), ..
                    }) = &m.default
                    {
                        self.bind_decl(body, false);
                    }
                }
            }
            hir::Decl::Impl(id) => {
                self.a
                    .name_refs
                    .push((id.tr.span, *id.tr.value(), Namespace::Type));
                id.tys.iter().for_each(|t| self.ty(t));
                id.context.iter().for_each(|b| self.bound(b));
                for (name, t) in &id.assocs {
                    self.a
                        .name_refs
                        .push((name.span, *name.value(), Namespace::Type));
                    self.ty(t);
                }
                for (_, body) in &id.methods {
                    self.bind_decl(body, false);
                }
            }

            hir::Decl::Mod(_) | hir::Decl::Use(_) | hir::Decl::Error => {}
        }
    }

    /// `Show a` in a `where`: a reference to the trait, and to what its type
    /// mentions.
    fn bound(&mut self, b: &hir::Bound) {
        self.a
            .name_refs
            .push((b.tr.span, *b.tr.value(), Namespace::Type));
        b.tys.iter().for_each(|t| self.ty(t));
    }

    /// A type or data constructor declaration.
    fn declare(&mut self, ns: Namespace, name: InternedString, span: Span) {
        let loc = Loc {
            source: self.source,
            span,
        };
        match ns {
            Namespace::Type => self.a.names.types.insert(name, loc),
            Namespace::Ctor => self.a.names.ctors.insert(name, loc),
        };
        // Standing on the declaration itself counts as referring to it, the way
        // it does for a value binding.
        self.a.name_refs.push((span, name, ns));
    }

    /// A resolved type expression: every constructor in it is a reference.
    fn ty(&mut self, t: &hir::LTypeExpr) {
        match t.value() {
            hir::TypeExpr::Con(name, args) => {
                self.a
                    .name_refs
                    .push((name.span, *name.value(), Namespace::Type));
                args.iter().for_each(|a| self.ty(a));
            }
            hir::TypeExpr::Fun(ps, r, eff) => {
                ps.iter().for_each(|p| self.ty(p));
                self.ty(r);
                if let Some(row) = eff {
                    for (label, args) in &row.labels {
                        args.iter().for_each(|a| self.ty(a));
                        let _ = label;
                    }
                }
            }
            hir::TypeExpr::Tuple(ts) => ts.iter().for_each(|x| self.ty(x)),
            hir::TypeExpr::Vector(x) | hir::TypeExpr::List(x) => self.ty(x),
            hir::TypeExpr::Record(fields, _) => fields.iter().for_each(|(_, x)| self.ty(x)),
            hir::TypeExpr::Row(row) => {
                for (_, args) in &row.labels {
                    args.iter().for_each(|x| self.ty(x));
                }
            }
            hir::TypeExpr::Var(_) | hir::TypeExpr::Error => {}
        }
    }

    /// `hint` is false at the top level: a `def`'s own name already shows its
    /// scheme on hover, and an inlay there would only repeat the signature.
    /// Record a top-level definition as a place a program could start.
    ///
    /// Only for the document being analysed, which is the only one with types
    /// to render, and only a `fun` or a `def` of a single name: `def (a, b) = …`
    /// has nothing one could call.
    fn function(&mut self, b: &hir::Bind) {
        let Some(types) = self.types else { return };
        let (ident, params) = match b {
            hir::Bind::Fun(name, params, _, _) => (name, params.len()),
            hir::Bind::Pat(p, _) => match p.value() {
                hir::Pat::Var(id) => (id, 0),
                _ => return,
            },
            hir::Bind::Error => return,
        };
        let text = &*self.source.content;
        let Some(name) = text.get(ident.span.start as usize..ident.span.end as usize) else {
            return;
        };
        let at = if params == 0 {
            match b {
                hir::Bind::Pat(p, _) => p.id,
                _ => ident.id,
            }
        } else {
            ident.id
        };
        let signature = types
            .get(at)
            .or_else(|| types.get(ident.id))
            .map(|t| {
                without_lone_effect(&meadow_compiler::infer::Renderer::for_table(types).render(t))
            })
            .unwrap_or_default();
        self.a.functions.push(Function {
            name: name.to_string(),
            var: *ident.value(),
            span: ident.span,
            test: None,
            params,
            signature,
        });
    }

    fn bind_decl(&mut self, b: &hir::Bind, hint: bool) {
        match b {
            hir::Bind::Fun(name, params, declared, body) => {
                self.bind(name, name.id, false);
                for p in params {
                    self.pat(p, true);
                }
                if let Some(t) = declared {
                    self.ty(t);
                }
                self.expr(body);
                // The result type, which nothing else shows: a top-level
                // binding hovers as its scheme, but a reader wants it on the
                // line. Only where there is a parameter list to put it after.
                // ...unless it is already written, in which case the hint
                // would be repeating what is on the line.
                if declared.is_none() {
                    if let (Some(last), Some(rendered)) =
                        (params.last(), self.result_hint(name.id, params.len()))
                    {
                        self.a.annotate_result(last.span, &rendered);
                    }
                }
            }
            hir::Bind::Pat(pat, expr) => {
                self.pat(pat, hint);
                self.expr(expr);
            }
            hir::Bind::Error => {}
        }
    }

    fn pat(&mut self, p: &hir::LPat, hint: bool) {
        self.record(p.span, p.id);
        match p.value() {
            hir::Pat::Var(ident) => self.bind(ident, ident.id, hint),
            // The annotation is written in the source, so it is a type
            // *reference* -- and the pattern under it needs no inlay hint,
            // because the type is already there to read.
            hir::Pat::Ann(inner, t) => {
                self.pat(inner, false);
                self.ty(t);
            }
            hir::Pat::As(ident, sub) => {
                self.bind(ident, ident.id, hint);
                self.pat(sub, false);
            }
            hir::Pat::Tuple(ps) | hir::Pat::List(ps) | hir::Pat::Array(ps) => {
                ps.iter().for_each(|q| self.pat(q, hint))
            }
            hir::Pat::Cons(name, ps) => {
                self.a
                    .name_refs
                    .push((name.span, *name.value(), Namespace::Ctor));
                ps.iter().for_each(|q| self.pat(q, hint));
            }
            hir::Pat::Record(fs, _) => fs.iter().for_each(|(_, q)| self.pat(q, hint)),
            hir::Pat::View(f, q) => {
                self.expr(f);
                self.pat(q, hint);
            }
            hir::Pat::Wildcard | hir::Pat::Lit(_) | hir::Pat::Unit | hir::Pat::Error => {}
        }
    }

    fn expr(&mut self, e: &hir::LExpr) {
        self.record(e.span, e.id);
        match e.value() {
            hir::Expr::Var(ident) => {
                self.a.refs.push((ident.span, *ident.value()));
                self.record(ident.span, ident.id);
            }
            hir::Expr::Lam(params, body) => {
                params.iter().for_each(|p| self.pat(p, true));
                self.expr(body);
            }
            hir::Expr::App(f, args) => {
                self.expr(f);
                args.iter().for_each(|a| self.expr(a));
            }
            hir::Expr::Let(binds, body) => {
                binds.iter().for_each(|b| self.bind_decl(b, true));
                self.expr(body);
            }
            hir::Expr::If(c, t, f) => {
                self.expr(c);
                self.expr(t);
                self.expr(f);
            }
            hir::Expr::Match(scrut, arms) => {
                self.expr(scrut);
                for (p, guard, arm) in arms {
                    self.pat(p, true);
                    if let Some(g) = guard {
                        self.expr(g);
                    }
                    self.expr(arm);
                }
            }
            hir::Expr::Tuple(xs) | hir::Expr::List(xs) | hir::Expr::Array(xs) => {
                xs.iter().for_each(|x| self.expr(x))
            }
            hir::Expr::Cons(name, xs) => {
                self.a
                    .name_refs
                    .push((name.span, *name.value(), Namespace::Ctor));
                xs.iter().for_each(|x| self.expr(x));
            }
            hir::Expr::Record(fs, base) => {
                fs.iter().for_each(|(_, x)| self.expr(x));
                if let Some(b) = base {
                    self.expr(b);
                }
            }
            hir::Expr::Field(x, _) => self.expr(x),
            hir::Expr::Update(base, fs) => {
                self.expr(base);
                fs.iter().for_each(|(_, x)| self.expr(x));
            }
            hir::Expr::Handle(body, arms, ret) => {
                self.expr(body);
                for arm in arms {
                    self.pat(&arm.param, true);
                    self.bind(&arm.resume, arm.resume.id, true);
                    self.expr(&arm.body);
                }
                if let Some((p, b)) = ret {
                    self.pat(p, true);
                    self.expr(b);
                }
            }
            hir::Expr::Lit(_) | hir::Expr::Unit | hir::Expr::Error => {}
        }
    }
}

// --- queries -----------------------------------------------------------------

impl Analysis {
    /// The type of the smallest node covering `offset`.
    ///
    /// Spans nest, so the tightest one wins: at the `x` in `f x`, that is `x`'s
    /// own type rather than the whole application's.
    pub fn type_at(&self, offset: usize) -> Option<&str> {
        self.typed
            .iter()
            .filter(|(s, _)| covers(*s, offset))
            .min_by_key(|(s, _)| s.end - s.start)
            .map(|(_, t)| t.as_str())
    }

    /// The binding mentioned at `offset`, if any.
    ///
    /// The smallest covering reference wins. An operator application records the
    /// operator itself over the *whole* expression — `answer + 1` is a mention of
    /// `+` spanning all of it — so the first match is routinely the wrong one.
    /// Among equally small candidates, prefer one defined in this file, since
    /// that is the one go-to-definition can actually take you to.
    pub fn var_at(&self, offset: usize) -> Option<VarId> {
        self.refs
            .iter()
            .filter(|(s, _)| covers(*s, offset))
            .min_by_key(|(s, v)| (s.end - s.start, !self.defs.contains_key(v)))
            .map(|(_, v)| *v)
            .or_else(|| {
                // Standing on the definition itself counts as referring to it.
                self.defs
                    .iter()
                    .filter(|(_, l)| covers(l.span, offset))
                    .min_by_key(|(_, l)| l.span.end - l.span.start)
                    .map(|(v, _)| *v)
            })
    }

    /// Where the binding under the cursor was introduced.
    ///
    /// `fallback` covers everything this document depends on. It is consulted
    /// second and is a plain map lookup, because a reference to an imported
    /// name already carries the defining module's `VarId` -- see [`DefIndex`].
    pub fn definition_at(
        &self,
        offset: usize,
        fallback: &DefIndex,
        declared: &NameIndex,
    ) -> Option<Loc> {
        // A value binding first: it is the only one of the three that is matched
        // by a resolved id rather than by a name, so when it answers, it is the
        // answer.
        if let Some(var) = self.var_at(offset) {
            // This document before the wider index: a local binding shadows an
            // imported one, and it is the local one the reference resolved to.
            if let Some(loc) = self
                .defs
                .get(&var)
                .or_else(|| self.wider.get(&var))
                .or_else(|| fallback.get(&var))
            {
                return Some(*loc);
            }
        }
        // Then a type or a data constructor. Innermost wins, so `Int` inside
        // `Maybe Int` beats the `Maybe` whose span encloses it.
        let (_, name, ns) = self
            .name_refs
            .iter()
            .filter(|(s, _, _)| covers(*s, offset))
            .min_by_key(|(s, _, _)| s.end - s.start)?;
        self.names
            .get(*name, *ns)
            .or_else(|| self.wider_names.get(*name, *ns))
            .or_else(|| declared.get(*name, *ns))
    }

    // --- rename --------------------------------------------------------------

    /// The identifier under `offset`, if it is one this server could rename.
    ///
    /// What an editor calls before showing the rename box: answering `None`
    /// keeps it from opening on something that cannot be renamed, which is
    /// better than opening and then failing.
    pub fn renameable_at(&self, offset: usize, std: &Std) -> Option<Span> {
        let (span, _) = self.rename_target(offset, std).ok()?;
        Some(span)
    }

    /// Every span a rename of the identifier under `offset` would rewrite.
    ///
    /// Exact for values, because a value reference carries the `VarId` it
    /// resolved to: two different `x`es are two ids, and the same `x` written
    /// in four modules is one. Types and data constructors have no ids, so
    /// they are matched by the name the resolver gave them -- which for a
    /// constructor is its canonical `Type.Ctor`, so `Expr.Int` and `Ty.Int`
    /// are still told apart.
    ///
    /// `None` in a result's first position means "this document"; `Some(src)`
    /// is another file of the package.
    pub fn rename_at(
        &self,
        offset: usize,
        new_name: &str,
        std: &Std,
    ) -> Result<Vec<(Option<Source>, Span)>, String> {
        let (_, target) = self.rename_target(offset, std)?;
        target.check_new_name(new_name)?;
        let mut edits: Vec<(Option<Source>, Span)> = Vec::new();
        match target {
            Target::Value(var, def) => {
                edits.push(def);
                for (span, v) in &self.refs {
                    if *v == var {
                        edits.push((None, *span));
                    }
                }
                for (src, span, v) in &self.wider_refs {
                    if *v == var {
                        edits.push((Some(*src), *span));
                    }
                }
            }
            Target::Name(name, ns, def) => {
                edits.push(def);
                for (span, n, k) in &self.name_refs {
                    if *n == name && *k == ns {
                        edits.push((None, *span));
                    }
                }
                for (src, span, n, k) in &self.wider_name_refs {
                    if *n == name && *k == ns {
                        edits.push((Some(*src), *span));
                    }
                }
            }
        }
        // A declaration is also a reference to itself (both walks record it),
        // and one span rewritten twice is a broken file.
        edits.sort_by_key(|(src, span)| (src.map(|s| s.id), span.start, span.end));
        edits.dedup();
        Ok(edits)
    }

    /// What the cursor is on, and where it was declared -- refusing anything
    /// this server has no business rewriting.
    fn rename_target(&self, offset: usize, std: &Std) -> Result<(Span, Target), String> {
        if let Some(var) = self.var_at(offset) {
            // The cursor is on a mention, or on the binding itself -- which is
            // where a rename is usually started from.
            let span = self
                .refs
                .iter()
                .filter(|(s, v)| *v == var && covers(*s, offset))
                .map(|(s, _)| *s)
                .next_back()
                .or_else(|| self.defs.get(&var).map(|l| l.span))
                .ok_or_else(|| "nothing to rename here".to_string())?;
            // Declared in this document, or in a sibling module of the same
            // package. Anywhere else -- the standard library, a dependency --
            // is a file this server is not editing.
            if let Some(loc) = self.defs.get(&var) {
                return Ok((span, Target::Value(var, (None, loc.span))));
            }
            if let Some(loc) = self.wider.get(&var) {
                return Ok((span, Target::Value(var, (Some(loc.source), loc.span))));
            }
            return Err(outside(std.definitions().contains_key(&var)));
        }
        let (span, name, ns) = self
            .name_refs
            .iter()
            .filter(|(s, _, _)| covers(*s, offset))
            .min_by_key(|(s, _, _)| s.end - s.start)
            .copied()
            .ok_or_else(|| "nothing to rename here".to_string())?;
        if let Some(loc) = self.names.get(name, ns) {
            return Ok((span, Target::Name(name, ns, (None, loc.span))));
        }
        if let Some(loc) = self.wider_names.get(name, ns) {
            return Ok((span, Target::Name(name, ns, (Some(loc.source), loc.span))));
        }
        Err(outside(std.declared_names().get(name, ns).is_some()))
    }

    /// Markdown for the hover: a signature, then any doc comment above it.
    ///
    /// A data constructor shows where it lives instead -- its full path, the
    /// way a `use` would name it -- since which `Int` a bare `Int` is matters
    /// more than a type the surrounding code already implies.
    pub fn hover_at(&self, offset: usize) -> Option<String> {
        let innermost = self
            .name_refs
            .iter()
            .filter(|(s, _, _)| covers(*s, offset))
            .min_by_key(|(s, _, _)| s.end - s.start);
        if let Some((_, name, Namespace::Ctor)) = innermost {
            let path = self
                .ctor_paths
                .get(name)
                .cloned()
                .unwrap_or_else(|| hir::ctor_spelling(&name.to_string()).to_string());
            return Some(format!("```meadow\n{path}\n```"));
        }
        let var = self.var_at(offset);
        let signature = match var.and_then(|v| self.schemes.get(&v)) {
            // A top-level binding shows its generalised scheme, which is more
            // informative than the type at this particular use site.
            Some(scheme) => {
                // A local binding is named by its binder, which is in this very
                // document -- the exports only name the top-level ones.
                let local = || {
                    let loc = self.defs.get(&var?)?;
                    let name = self
                        .source
                        .get(loc.span.start as usize..loc.span.end as usize)?;
                    Some(name.to_string())
                };
                let name = var
                    .and_then(|v| self.binding_names.get(&v).cloned())
                    .or_else(local);
                match name {
                    Some(n) => format!("{} : {scheme}", hir::spell_name(&n)),
                    None => scheme.clone(),
                }
            }
            None => {
                let ty = self.type_at(offset)?;
                match var.and_then(|v| self.binding_names.get(&v)) {
                    Some(n) => format!("{} : {ty}", hir::spell_name(n)),
                    None => ty.to_string(),
                }
            }
        };

        let mut out = format!("```meadow\n{signature}\n```");
        if let Some(doc) = var
            .and_then(|v| self.defs.get(&v))
            .and_then(|l| self.doc_above(l.span))
        {
            out.push_str("\n\n---\n\n");
            out.push_str(&doc);
        }
        Some(out)
    }

    /// The `--` comment block immediately above the line `span` starts on.
    ///
    /// Comments are discarded by the lexer, so documentation is recovered from
    /// the source text — the run of comment lines directly above a definition,
    /// with the leading `--` stripped.
    pub fn doc_above(&self, span: Span) -> Option<String> {
        let before = self.source.get(..span.start as usize)?;
        let mut lines: Vec<&str> = Vec::new();
        // The line the definition starts on is partial; skip it.
        for line in before.lines().rev().skip(1) {
            let t = line.trim();
            if t.is_empty() && lines.is_empty() {
                continue;
            }
            match t.strip_prefix("--") {
                Some(rest) => lines.push(rest.trim()),
                None => break,
            }
        }
        if lines.is_empty() {
            return None;
        }
        lines.reverse();
        Some(lines.join("\n"))
    }
}

fn covers(s: Span, offset: usize) -> bool {
    (s.start as usize) <= offset && offset < (s.end as usize)
}

/// Type and constructor names declared by `decls`.
fn collect_names(
    decls: &[hir::LDecl],
    types: &mut std::collections::HashSet<String>,
    ctors: &mut std::collections::HashSet<String>,
) {
    for d in decls {
        match d.value() {
            hir::Decl::Data(dd) => {
                types.insert(hir::spelling(&dd.name).to_string());
                // Canonical in the HIR (`Maybe.Just`); highlighting matches
                // against the bare word the source actually contains.
                ctors.extend(dd.variants.iter().map(|v| {
                    let n = v.name.to_string();
                    n.rsplit_once('.').map(|(_, c)| c.to_string()).unwrap_or(n)
                }));
            }
            hir::Decl::Record(rd) => {
                types.insert(hir::spelling(&rd.name).to_string());
                ctors.insert(hir::spelling(&rd.name).to_string());
            }
            hir::Decl::Effect(ed) => {
                types.insert(hir::spelling(&ed.name).to_string());
            }
            hir::Decl::Alias(ad) => {
                types.insert(hir::spelling(&ad.name).to_string());
            }
            _ => {}
        }
    }
}

impl Analysis {
    /// An empty analysis, for a walk that only wants the definitions out of it.
    fn empty() -> Analysis {
        Analysis {
            source: String::new(),
            diagnostics: Vec::new(),
            typed: Vec::new(),
            typed_nodes: Vec::new(),
            candidates: Vec::new(),
            defs: Default::default(),
            refs: Vec::new(),
            name_refs: Vec::new(),
            names: Default::default(),
            binders: Vec::new(),
            binding_names: Default::default(),
            schemes: Default::default(),
            types_in_scope: Default::default(),
            ctors_in_scope: Default::default(),
            wider: Default::default(),
            wider_names: Default::default(),
            wider_refs: Vec::new(),
            wider_name_refs: Vec::new(),
            functions: Vec::new(),
            ctor_paths: Default::default(),
            paths: Default::default(),
            declarations: Vec::new(),
            ir: Default::default(),
            source_id: 0,
        }
    }
}

impl Analysis {
    /// Hint `(name : ty)` around the binder at `span`.
    ///
    /// Two hints, not one: an opening parenthesis before the name and the
    /// annotation after it. The parentheses are not decoration — `(x : Int)` is
    /// the syntax, so the hint reads as something that could be typed in its
    /// place rather than as a notation of its own.
    fn annotate(&mut self, span: Span, rendered: &str) {
        self.binders.push(Hint {
            offset: span.start,
            parts: vec![HintPart::Text("(".to_string())],
        });
        let mut parts = vec![HintPart::Text(" : ".to_string())];
        parts.extend(hint_parts(rendered));
        parts.push(HintPart::Text(")".to_string()));
        self.binders.push(Hint {
            offset: span.end,
            parts,
        });
    }

    /// Hint the result type of a function, after its last parameter.
    ///
    /// Appended to the hint already sitting there when there is one, so that
    /// `fun add x y` reads `fun add (x : Int) (y : Int) : Int` rather than
    /// producing two hints at one position whose order is the client's to
    /// decide. A parameter that is itself parenthesised — `fun f (a, b)` —
    /// closes before its own span ends, so nothing is there to append to and
    /// the result gets a hint of its own.
    fn annotate_result(&mut self, after: Span, rendered: &str) {
        let mut parts = vec![HintPart::Text(" : ".to_string())];
        parts.extend(hint_parts(rendered));
        match self.binders.iter_mut().find(|h| h.offset == after.end) {
            Some(h) => h.parts.extend(parts),
            None => self.binders.push(Hint {
                offset: after.end,
                parts,
            }),
        }
    }
}

/// How many times the type variable `v` appears in `ty`.
///
/// Used to decide whether a function's latent effect is worth printing — see
/// [`Walk::result_hint`].
fn occurrences(ty: &meadow_compiler::infer::Type, v: u32) -> usize {
    use meadow_compiler::infer::Type::*;
    match ty {
        Var(x) => usize::from(*x == v),
        Bound(_) | RowEmpty | Error => 0,
        Con(_, args) | Tuple(args) => args.iter().map(|t| occurrences(t, v)).sum(),
        Fun(ps, r, e) => {
            ps.iter().map(|t| occurrences(t, v)).sum::<usize>()
                + occurrences(r, v)
                + occurrences(e, v)
        }
        Record(r) => occurrences(r, v),
        RowExtend(_, f, rest) => occurrences(f, v) + occurrences(rest, v),
    }
}
