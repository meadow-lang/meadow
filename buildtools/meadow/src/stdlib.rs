//! The embedded **`Std`** package.
//!
//! `lib/Std/` on disk is the source of truth; its modules are `include_str!`d here
//! so the compiler always has them, with no filesystem lookup. [`compile_std`]
//! builds them into a single [`CompiledPackage`] that [`crate::pipeline::build`]
//! and the `meadow` crate inject as an implicit dependency of everything else — so the
//! prelude's names (`map`, `foldl`, `Option`, `Bool`, …) are in scope everywhere
//! without an explicit `use`.
//!
//! There is no qualified `use` yet, so every top-level name across these modules
//! shares one flat namespace and must be unique. Each module marks its public API
//! `@pub`; `Prelude` defines nothing and only `@pub use`-re-exports a curated set
//! (including `map` / `filter` / `foldl` / `foldr` / … which are deliberately
//! *not* `@pub` in `Std.Collections.List`, so re-export is what makes them
//! public).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use meadow_compiler::{
    AstModule, CompiledPackage, Export, Options, compile_unit_in_package,
    diagnostics::{self, Diagnostic},
    infer::TypeTable,
    intern::InternedString,
    lexer::tokenize,
    parser,
    source::{Source, SourceKind},
};
use std::collections::HashMap;
use std::path::PathBuf;

/// The package name shown in `:module` / linker dumps, and the `Std` in
/// `Std.Collections.List`.
pub const PACKAGE_NAME: &str = "Std";

/// `(dotted module path, source)`. Each is compiled as its own unit, in this
/// order, with the earlier ones as dependencies — a module sees another only
/// through an explicit `use`. `Prelude` is last and only re-exports; its
/// re-exports are the names a dependent package gets unqualified.
pub const MODULES: &[(&str, &str)] = &[
    ("Lib", include_str!("../../../lib/Std/src/Lib.mw")),
    // First, and dependency-free, so every module below can hold `@test`s.
    ("Ops", include_str!("../../../lib/Std/src/Ops.mw")),
    ("Display", include_str!("../../../lib/Std/src/Display.mw")),
    ("Debug", include_str!("../../../lib/Std/src/Debug.mw")),
    ("Test", include_str!("../../../lib/Std/src/Test.mw")),
    ("Bool", include_str!("../../../lib/Std/src/Bool.mw")),
    ("Ref", include_str!("../../../lib/Std/src/Ref.mw")),
    ("Ordering", include_str!("../../../lib/Std/src/Ordering.mw")),
    ("Function", include_str!("../../../lib/Std/src/Function.mw")),
    ("Tuple", include_str!("../../../lib/Std/src/Tuple.mw")),
    ("Num", include_str!("../../../lib/Std/src/Num.mw")),
    ("Num.Int", include_str!("../../../lib/Std/src/Num/Int.mw")),
    ("Maybe", include_str!("../../../lib/Std/src/Maybe.mw")),
    // After the types its methods answer with; everything below compares
    // with it.
    ("Cmp", include_str!("../../../lib/Std/src/Cmp.mw")),
    ("Char", include_str!("../../../lib/Std/src/Char.mw")),
    ("Result", include_str!("../../../lib/Std/src/Result.mw")),
    ("Num.Bits", include_str!("../../../lib/Std/src/Num/Bits.mw")),
    ("Num.Float", include_str!("../../../lib/Std/src/Num/Float.mw")),
    ("Bytes", include_str!("../../../lib/Std/src/Bytes.mw")),
    ("Yield", include_str!("../../../lib/Std/src/Yield.mw")),
    (
        "Collections",
        include_str!("../../../lib/Std/src/Collections.mw"),
    ),
    (
        "Collections.Vector",
        include_str!("../../../lib/Std/src/Collections/Vector.mw"),
    ),
    (
        "Collections.List",
        include_str!("../../../lib/Std/src/Collections/List.mw"),
    ),
    (
        "Collections.Tree",
        include_str!("../../../lib/Std/src/Collections/Tree.mw"),
    ),
    (
        "Collections.Set",
        include_str!("../../../lib/Std/src/Collections/Set.mw"),
    ),
    (
        "Collections.Map",
        include_str!("../../../lib/Std/src/Collections/Map.mw"),
    ),
    ("Either", include_str!("../../../lib/Std/src/Either.mw")),
    ("St", include_str!("../../../lib/Std/src/St.mw")),
    ("Sort", include_str!("../../../lib/Std/src/Sort.mw")),
    (
        "Collections.HashMap",
        include_str!("../../../lib/Std/src/Collections/HashMap.mw"),
    ),
    (
        "Collections.HashTable",
        include_str!("../../../lib/Std/src/Collections/HashTable.mw"),
    ),
    (
        "Collections.UnionFind",
        include_str!("../../../lib/Std/src/Collections/UnionFind.mw"),
    ),
    ("Compact", include_str!("../../../lib/Std/src/Compact.mw")),
    ("Thread", include_str!("../../../lib/Std/src/Thread.mw")),
    ("Stm", include_str!("../../../lib/Std/src/Stm.mw")),
    ("State", include_str!("../../../lib/Std/src/State.mw")),
    ("Exn", include_str!("../../../lib/Std/src/Exn.mw")),
    ("Stream", include_str!("../../../lib/Std/src/Stream.mw")),
    ("Random", include_str!("../../../lib/Std/src/Random.mw")),
    ("Fs", include_str!("../../../lib/Std/src/Fs.mw")),
    ("Process", include_str!("../../../lib/Std/src/Process.mw")),
    ("String", include_str!("../../../lib/Std/src/String.mw")),
    (
        "String.Parse",
        include_str!("../../../lib/Std/src/String/Parse.mw"),
    ),
    (
        "String.Parse.Char",
        include_str!("../../../lib/Std/src/String/Parse/Char.mw"),
    ),
    (
        "String.Parse.Lexer",
        include_str!("../../../lib/Std/src/String/Parse/Lexer.mw"),
    ),
    // After `Random`, `Collections.Vector` and `String`, which a generator is
    // written with: the one part of the test library that is not first.
    (
        "Test.Property",
        include_str!("../../../lib/Std/src/Test/Property.mw"),
    ),
    // After `Test.Property`, whose generators make what it tries.
    (
        "Test.Fuzz",
        include_str!("../../../lib/Std/src/Test/Fuzz.mw"),
    ),
    ("Path", include_str!("../../../lib/Std/src/Path.mw")),
    ("Json", include_str!("../../../lib/Std/src/Json.mw")),
    // After `String`, whose `join` it writes tokens with.
    ("Macro", include_str!("../../../lib/Std/src/Macro.mw")),
    // After `Macro`, whose tokens it reads, and `String.Parse`, which reads them.
    (
        "Macro.Parse",
        include_str!("../../../lib/Std/src/Macro/Parse.mw"),
    ),
    ("Console", include_str!("../../../lib/Std/src/Console.mw")),
    // After `Console`, which a macro run reads its call from and writes its
    // answer to, and `Macro`, whose tokens it carries.
    (
        "Macro.Serve",
        include_str!("../../../lib/Std/src/Macro/Serve.mw"),
    ),
    // After `Console`, whose operations it is written over.
    ("Terminal", include_str!("../../../lib/Std/src/Terminal.mw")),
    // After `Process`, whose operations it is written over.
    ("Ffi", include_str!("../../../lib/Std/src/Ffi.mw")),
    // After `Console`, so `time` can print what it measured.
    ("Time", include_str!("../../../lib/Std/src/Time.mw")),
    ("Log", include_str!("../../../lib/Std/src/Log.mw")),
    ("Net", include_str!("../../../lib/Std/src/Net.mw")),
    ("Tls", include_str!("../../../lib/Std/src/Tls.mw")),
    ("Eff", include_str!("../../../lib/Std/src/Eff.mw")),
    ("Http", include_str!("../../../lib/Std/src/Http.mw")),
    ("Bench", include_str!("../../../lib/Std/src/Bench.mw")),
    ("Prelude", include_str!("../../../lib/Std/src/Prelude.mw")),
];

/// Each `Std` module compiled as its own unit, in dependency order, paired with
/// the dotted name `MODULES` spells it with.
///
/// [`std_packages`] is the bundle a dependent sees; this is what that bundle was
/// made from. The language server needs the pieces: a file that *is* a `Std`
/// module has to be analysed in its own place — against the modules before it —
/// or every type and constructor it declares collides with the copy sitting in
/// its own dependency, and the editor fills with `already defined`.
pub fn std_modules(opts: Options) -> (Vec<(&'static str, CompiledPackage)>, Vec<Diagnostic>) {
    type Cache = OnceLock<(Vec<(&'static str, CompiledPackage)>, Vec<Diagnostic>)>;
    static CACHE: [Cache; 4] = [const { OnceLock::new() }; 4];
    // Keyed on strictness and on debug information, and only on those: the
    // optimisation level is a *back end* concern — decision trees happen in
    // `meadow_seq`, after a `CompiledPackage` exists — so it cannot change
    // what is cached here. Debug information can: it is source positions in
    // the lowered `core`. Anything else that changes a `CompiledPackage` has
    // to join the key, and the number of `Std` compiles per process grows
    // with it.
    let cell = &CACHE[cache_key(opts)];
    let (modules, diags) = cell.get_or_init(|| {
        counter(opts).fetch_add(1, Ordering::Relaxed);
        if let Some(modules) = precompiled(opts).or_else(|| cached(opts)) {
            return (modules, Vec::new());
        }
        let (modules, diags) = compile_modules(opts);
        if diags.is_empty() {
            save(opts, &modules);
        }
        (modules, diags)
    });
    (modules.clone(), diags.clone())
}

// --- the standard library, compiled ahead of time --------------------------
//
// `Std` is the same for every program, so like rustup's prebuilt `std` it is
// compiled once, not once per project: a released `meadow` carries it
// compiled, built into the binary (`build.rs`, `__precompile-std`), and a
// `meadow` built without it compiles it on first use and keeps the result
// under `~/.meadow/lib/std/`, for every project after. Either way nobody sees
// it compiled -- it is the toolchain's, not the build's.
//
// What is kept is the modules, in the form [`std_modules`] answers: the bundle
// a dependent sees is made from them in a moment, and the language server
// needs them apart. It is the front end's output, from which both back ends --
// bytecode and native -- start. A compile of `Std` depends on the platform,
// on whether `match` must be exhaustive and on debug information (see
// [`cache_key`]), so there is one per combination, and `MEADOW_STD_FINGERPRINT`
// -- a hash of the sources that produce it -- says which compiler made it.

/// The compiled modules built into this binary for `opts`, if there are any.
fn precompiled(opts: Options) -> Option<Vec<(&'static str, CompiledPackage)>> {
    static BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/std"));
    if BLOB.is_empty() {
        return None;
    }
    let (fingerprint, entries): Prebuilt = postcard::from_bytes(BLOB).ok()?;
    if fingerprint != FINGERPRINT {
        return None;
    }
    let key = variant(opts);
    let (_, modules) = entries.iter().find(|(v, _)| *v == key)?;
    decode(modules)
}

/// The modules an earlier run of this toolchain compiled for `opts`.
fn cached(opts: Options) -> Option<Vec<(&'static str, CompiledPackage)>> {
    // Each `meadow` built from other sources keeps its own compile here, so
    // this is where they pile up -- see `cache`.
    crate::cache::auto();
    let path = cache_file(opts)?;
    let modules = decode(&std::fs::read(&path).ok()?)?;
    crate::cache::used(path.parent()?);
    Some(modules)
}

/// Keep `modules` for every later run of this toolchain. Whole or not at all,
/// since another `meadow` may be reading it.
fn save(opts: Options, modules: &[(&'static str, CompiledPackage)]) {
    let Some(path) = cache_file(opts) else { return };
    let Some(bytes) = encode(modules) else { return };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let partial = path.with_extension(format!("{}", std::process::id()));
    if std::fs::write(&partial, &bytes).is_ok() && std::fs::rename(&partial, &path).is_err() {
        let _ = std::fs::remove_file(&partial);
    }
}

fn cache_file(opts: Options) -> Option<PathBuf> {
    Some(
        home()?
            .join("lib")
            .join("std")
            .join(FINGERPRINT)
            .join(format!("Std-{}.mstd", variant(opts))),
    )
}

/// The hash of the sources that produce a compiled `Std` -- see `build.rs`.
const FINGERPRINT: &str = env!("MEADOW_STD_FINGERPRINT");

/// The fingerprint of the sources the standard library is built from: what
/// tells one `Std` from another, whoever compiled it.
pub fn fingerprint() -> &'static str {
    FINGERPRINT
}

/// Which compile of `Std` `opts` wants: the platform, and [`cache_key`].
fn variant(opts: Options) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("{:?}", opts.cfg.platform()).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}-{}", cache_key(opts))
}

/// What `__precompile-std` writes and `build.rs` embeds: the fingerprint, and
/// each variant's modules as [`encode`] writes them -- decoded only for the one
/// a build wants.
type Prebuilt = (String, Vec<(String, Vec<u8>)>);

const MAGIC: &[u8; 8] = b"MWSTD\x00\x00\x01";

fn encode(modules: &[(&'static str, CompiledPackage)]) -> Option<Vec<u8>> {
    let named: Vec<(&str, &CompiledPackage)> = modules.iter().map(|(n, p)| (*n, p)).collect();
    let payload = postcard::to_stdvec(&named).ok()?;
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(FINGERPRINT.as_bytes());
    bytes.extend_from_slice(&payload);
    Some(bytes)
}

/// The modules in `bytes`, if [`encode`] wrote them with this fingerprint --
/// and if they are the modules `MODULES` names, in its order.
fn decode(bytes: &[u8]) -> Option<Vec<(&'static str, CompiledPackage)>> {
    let rest = bytes.strip_prefix(MAGIC)?;
    let rest = rest.strip_prefix(FINGERPRINT.as_bytes())?;
    let named: Vec<(String, CompiledPackage)> = postcard::from_bytes(rest).ok()?;
    if named.len() != MODULES.len() {
        return None;
    }
    named
        .into_iter()
        .zip(MODULES)
        .map(|((n, p), (dotted, _))| (n == *dotted).then_some((*dotted, p)))
        .collect()
}

/// Compile `Std` for `target` -- a Rust target triple, or the machine this
/// runs on -- every variant a build can ask for, into one file for `build.rs`
/// to embed: what `meadow __precompile-std` does. A release cross-compiles
/// most of its `meadow`s, which cannot run where they are built, so the one
/// that can compiles `Std` for each of them.
pub fn precompile(to: &std::path::Path, target: Option<&str>) -> Result<(), String> {
    let platform = match target {
        Some(triple) => Some(platform_of(triple)?),
        None => None,
    };
    let mut entries = Vec::new();
    for strict in [false, true] {
        for debug_info in [false, true] {
            let mut opts = if strict {
                Options::release()
            } else {
                Options::debug()
            };
            opts.debug_info = debug_info;
            if let Some((os, arch)) = platform {
                opts.cfg = opts.cfg.on(os, arch);
            }
            let (modules, diags) = compile_modules(opts);
            if !diags.is_empty() {
                return Err(format!(
                    "the standard library does not compile: {}",
                    diags[0].msg
                ));
            }
            entries.push((
                variant(opts),
                encode(&modules).ok_or("could not encode the standard library")?,
            ));
        }
    }
    let prebuilt: Prebuilt = (FINGERPRINT.to_string(), entries);
    let blob = postcard::to_stdvec(&prebuilt).map_err(|e| e.to_string())?;
    std::fs::write(to, blob).map_err(|e| format!("could not write {}: {e}", to.display()))
}

/// What `std::env::consts` says on the machine a Rust target triple is for:
/// what a `meadow` built for it will look its `Std` up by.
fn platform_of(triple: &str) -> Result<(&'static str, &'static str), String> {
    let arch = match triple.split('-').next() {
        Some("x86_64") => "x86_64",
        Some("aarch64") => "aarch64",
        _ => return Err(format!("no `Std` for `{triple}`: x86_64 and aarch64 only")),
    };
    let os = if triple.contains("windows") {
        "windows"
    } else if triple.contains("android") {
        "android"
    } else if triple.contains("apple-darwin") {
        "macos"
    } else if triple.contains("linux") {
        "linux"
    } else {
        return Err(format!("no `Std` for `{triple}`: an unknown system"));
    };
    Ok((os, arch))
}

/// The embedded `Std` package, compiled once per process.
///
/// [`compile_modules`] takes about five seconds and is deterministic, so
/// compiling it twice in one process is pure waste — and something did exactly
/// that on every REPL line, every language-server keystroke, and once per test,
/// which is what made the test suite take minutes rather than seconds.
///
/// Cached per profile: `--release` turns on the exhaustiveness check and can
/// report different diagnostics. The result is cloned rather than shared,
/// because linking consumes its packages — and cloning the compiled tree is
/// about two orders of magnitude cheaper than rebuilding it.
pub fn std_packages(opts: Options) -> (Vec<CompiledPackage>, Vec<Diagnostic>) {
    static CACHE: [OnceLock<(Vec<CompiledPackage>, Vec<Diagnostic>)>; 4] =
        [const { OnceLock::new() }; 4];
    // The same key as `std_modules`, for the reason given there.
    let cell = &CACHE[cache_key(opts)];
    let (packages, diags) = cell.get_or_init(|| {
        // Shares the one compile with `std_modules`, so asking for both costs
        // memory but not time.
        let (modules, diags) = std_modules(opts);
        let subs = modules.into_iter().map(|(_, p)| p).collect();
        (
            vec![bundle(InternedString::from(PACKAGE_NAME), subs)],
            diags,
        )
    });
    (packages.clone(), diags.clone())
}

/// The module path a dotted name sits at within the package.
///
/// `Lib` and `Prelude` are at the root: they are what a dependent gets
/// unqualified. Everything else is nested, so a sibling reaches it only through
/// an explicit `use`.
pub fn module_path(dotted: &str) -> Vec<InternedString> {
    if dotted == "Prelude" || dotted == "Lib" {
        Vec::new()
    } else {
        dotted.split('.').map(InternedString::from).collect()
    }
}

/// Which cached `Std` a set of options gets -- see [`std_modules`].
fn cache_key(opts: Options) -> usize {
    usize::from(opts.check_exhaustive()) * 2 + usize::from(opts.debug_info)
}

fn counter(opts: Options) -> &'static AtomicUsize {
    static COUNTS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
    // The same key the caches use.
    &COUNTS[cache_key(opts)]
}

/// How many times [`std_packages`] has actually compiled `Std` for this profile
/// -- or read it back from an earlier process's, which is the same work saved.
///
/// At most one, for the life of the process — that is the whole point of the
/// cache, and it is what `tests/stdlib.rs` asserts. It used to assert it by
/// timing two calls against each other, which is not a fact about the cache: the
/// tests in that binary run in parallel and most of them warm it first, so the
/// "cold" measurement was a second clone, and on a loaded machine the two were
/// indistinguishable noise. A count is the property itself.
pub fn compiles(opts: Options) -> usize {
    counter(opts).load(Ordering::Relaxed)
}

/// Compile each embedded `Std` module as its own unit, in dependency order.
///
/// Returns them paired with their dotted names plus any diagnostics — which for
/// a healthy tree is empty. Callers surface the diagnostics
/// ([`crate::pipeline::build`] and the REPL print them; `tests/stdlib.rs` asserts
/// they stay empty).
/// `Std` compiled afresh, past every cache: for a tool that wants to watch
/// it being compiled -- what its macros expand to, say
/// (`meadow_compiler::expand::record`).
pub fn compile_fresh(opts: Options) -> (Vec<(&'static str, CompiledPackage)>, Vec<Diagnostic>) {
    compile_modules(opts)
}

fn compile_modules(opts: Options) -> (Vec<(&'static str, CompiledPackage)>, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let pkg = InternedString::from(PACKAGE_NAME);

    // Compile each module as its own unit, in order, with the ones already done as
    // dependencies. Each sub-unit gets `prelude_exports = Some([])` so a later
    // sibling only reaches it through `use`.
    let mut subs: Vec<(&'static str, CompiledPackage)> = Vec::new();
    for (index, (dotted, src)) in MODULES.iter().enumerate() {
        let filename = format!("Std/{}.mw", dotted.replace('.', "/"));
        // Known by its place in the library, in every process: this compile is
        // kept, and read back by others -- see `meadow_source::FIXED_IDS`.
        let source = Source::fixed(
            index as u32,
            SourceKind::File(InternedString::from(filename.as_str())),
            InternedString::from(*src),
        );
        let lex = tokenize(source);
        diags.extend(lex.errors);

        let path = module_path(dotted);
        let mname = path
            .last()
            .copied()
            .unwrap_or_else(|| InternedString::from(*dotted));

        let (ast, perrs) = parser::parse(mname, source, &lex.tokens);
        for e in &perrs {
            diags.push(diagnostics::from_parse_error(&filename, e));
        }
        let Some(ast) = ast else { continue };

        let dep_refs: Vec<meadow_compiler::Dep<'_>> = subs
            .iter()
            .map(|(_, p)| meadow_compiler::Dep::new(p))
            .collect();
        let (mut cp, unit_diags) = compile_unit_in_package(
            pkg,
            InternedString::from(*dotted),
            subs.len(),
            vec![AstModule {
                path: path.clone(),
                name: mname,
                ast,
                source,
            }],
            &dep_refs,
            // One `Std` serves every build in the process, so it sees the
            // platform and nothing else: no profile, backend, test or flag.
            Options {
                cfg: opts.cfg.platform(),
                ..opts
            },
        );
        diags.extend(unit_diags);
        // A sibling reaches this module only via `use`, never flat.
        cp.prelude_exports = Some(Vec::new());
        // Tag exports with the module path so `use Std.a.b (x)` from outside works.
        let flat = cp.exports.clone();
        for e in &mut cp.exports {
            e.module = path.clone();
        }
        // Except the operators and `display`, which every later module has
        // unqualified, and so -- being at the root in the bundle -- does every
        // dependent.
        if matches!(*dotted, "Ops" | "Display" | "Debug" | "Cmp") {
            cp.prelude_exports = Some(flat.iter().map(|e| e.name).collect());
            // And their traits, which a program asks for and implements
            // without a `use`: `where Eq a`, `impl Display T`.
            cp.flat_types = cp
                .data_decls
                .iter()
                .filter_map(|d| match d.value() {
                    meadow_compiler::hir::Decl::Trait(td) => Some(td.name),
                    _ => None,
                })
                .collect();
            // At the root, which is what a flat import takes: a trait's
            // methods are there already, and its functions have to be put.
            cp.exports.extend(flat.into_iter().map(|mut e| {
                e.module = Vec::new();
                e
            }));
        }
        subs.push((dotted, cp));
    }

    (subs, diags)
}

/// Fold the separately-compiled `Std` modules into one package. The `Prelude`
/// module's exports (path `[]`) become the names a dependent gets unqualified.
fn bundle(name: InternedString, subs: Vec<CompiledPackage>) -> CompiledPackage {
    // The sub-units were compiled in a chain, each above the last, so the
    // bundle's range is simply the span of all of them.
    let lo = subs.iter().map(|s| s.vars.start).min().unwrap_or(0);
    let hi = subs.iter().map(|s| s.vars.end).max().unwrap_or(0);
    let mut defs = Vec::new();
    let mut modules = Vec::new();
    let mut ctor_fields = HashMap::new();
    let mut variants = HashMap::new();
    let mut data_decls = Vec::new();
    let mut types = TypeTable::default();
    let mut exports: Vec<Export> = Vec::new();
    let mut generalized = HashMap::new();
    let mut prelude_names: Vec<InternedString> = Vec::new();
    // Unioned across the sub-units, which in practice means `Prelude.mw`'s:
    // it is the only one that `@pub use`s a type's constructors.
    let mut flat_ctors: Vec<InternedString> = Vec::new();
    // And the types it names: `@pub use Std.Maybe (Maybe)`.
    let mut flat_types: Vec<InternedString> = Vec::new();
    let mut tests = Vec::new();
    // What the library itself embedded, so a build notices those files too.
    let mut embedded: Vec<(String, u64)> = Vec::new();
    // The macros of every module, which in the bundle are the library's.
    let mut macros = Vec::new();
    // And their compile-time bindings, which are the library's too.
    let mut bindings = Vec::new();
    // Every sub-unit's, each of which has its predecessors' too.
    let mut fixities = Vec::new();
    let mut compacting = Vec::new();

    for sub in subs {
        compacting.extend(sub.compacting.iter().copied());
        fixities.extend(sub.fixities.iter().copied());
        embedded.extend(sub.embedded.iter().cloned());
        macros.extend(sub.macros.iter().cloned());
        bindings.extend(sub.bindings.iter().cloned());
        flat_ctors.extend(sub.flat_ctors.iter().copied());
        flat_types.extend(sub.flat_types.iter().copied());
        defs.extend(sub.defs);
        modules.extend(sub.modules);
        ctor_fields.extend(sub.ctor_fields);
        variants.extend(sub.variants);
        data_decls.extend(sub.data_decls);
        tests.extend(sub.tests);
        types.absorb(sub.types);
        generalized.extend(sub.generalized);
        for e in sub.exports {
            if e.module.is_empty() {
                prelude_names.push(e.name);
            }
            exports.push(e);
        }
    }

    flat_ctors.sort_by_key(|n| n.to_string());
    flat_ctors.dedup();
    flat_types.sort_by_key(|n| n.to_string());
    flat_types.dedup();
    fixities.sort_by_key(|(op, _)| op.to_string());
    fixities.dedup();

    CompiledPackage {
        id: 0,
        flat_ctors,
        flat_types,
        vars: lo..hi,
        name,
        // The standard library is one package, at one version, in every build:
        // nothing has to tell two copies of it apart.
        ident: name,
        macros,
        bindings,
        embedded,
        fixities,
        // A library has no entry point, and `Std` least of all.
        entry: None,
        value_entry: None,
        modules,
        types,
        exports,
        generalized,
        defs,
        ctor_fields,
        variants,
        data_decls,
        tests,
        prelude_exports: Some(prelude_names),
        package: false,
        compacting,
    }
}

/// Write the embedded sources to disk, and answer the directory they went to.
///
/// The library is compiled from text baked into the binary, so its modules'
/// [`SourceKind::File`] labels — `Std/Collections/Vector.mw` — name nothing an
/// editor could open. Go-to-definition into `Std` needs a real file, so here is
/// one, laid out under `<home>/std/<version>` exactly as those labels spell it.
///
/// Versioned because the sources belong to *this* binary: `meadow update`
/// installs a new one beside the old, and a stale `Vector.mw` would send an
/// editor to the wrong line rather than to no line at all. Rewritten whenever
/// what is on disk differs from what is embedded, so a half-written directory
/// repairs itself and a hand-edited one does not persist.
///
/// Best-effort by design. A read-only home, a sandbox, a full disk — none of
/// those should stop the language server from starting, so the caller gets
/// `None` and simply loses navigation into the library.
pub fn extract_sources() -> Option<PathBuf> {
    let root = home()?.join("std").join(env!("CARGO_PKG_VERSION"));
    for (dotted, src) in MODULES {
        let path = root.join(format!("Std/{}.mw", dotted.replace('.', "/")));
        // Compare before writing: this runs at every editor start, and the
        // common case is that everything is already correct.
        if std::fs::read_to_string(&path).is_ok_and(|on_disk| on_disk == **src) {
            continue;
        }
        std::fs::create_dir_all(path.parent()?).ok()?;
        // Remove first: the copy already there is read-only, and on Windows
        // that makes it unwritable rather than merely discouraging.
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, src).ok()?;
        read_only(&path);
    }
    Some(root)
}

/// Mark an extracted source read-only.
///
/// These are a *copy* of the library, and the person most likely to follow a
/// definition into one is the person who maintains the original — for whom
/// editing the copy would mean losing the work silently. A read-only file turns
/// that into the editor saying so.
///
/// Ignored if it fails; it is a courtesy, not a guarantee.
fn read_only(path: &std::path::Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_readonly(true);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// Where Meadow keeps things that are not the binary. Mirrors the installer's
/// `default_home`, including the `MEADOW_HOME` override.
pub(crate) fn home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("MEADOW_HOME") {
        return Some(PathBuf::from(dir));
    }
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(PathBuf::from(base).join(".meadow"))
}
