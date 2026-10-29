//! What the language server needs from the build system.
//!
//! `meadow-lsp` answers an editor; this crate knows how packages are laid out
//! on disk. The arrow between them points one way — this crate depends on the
//! server for its `lsp` subcommand — so the server asks for a package through a
//! function pointer, and this module is what it is given.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

/// Find the package a file belongs to, for the language server.
///
/// The server cannot do this itself: manifest parsing and module discovery live
/// here, and `meadow-lsp` is a dependency of this crate rather than the other
/// way round. So it asks through a function pointer, and this is the function.
///
/// Its **dependencies are built too**, and that is the part worth explaining.
/// Doing it on every keystroke would be a full build per keystroke, so what a
/// package's dependencies compiled to is kept, and reused until one of their
/// sources changes. Without them, every name a dependency provides is
/// undefined -- which is what an editor used to show for any package that had
/// one.
pub fn load_package(file: &std::path::Path) -> Option<meadow_lsp::analysis::PackageSources> {
    find_package(file)?.ok()
}

/// What a package's dependencies came to, and what can run a macro one of them
/// exports.
struct Built {
    deps: Vec<(
        meadow_compiler::intern::InternedString,
        Rc<meadow_compiler::CompiledPackage>,
    )>,
    procs: Rc<dyn meadow_compiler::expand::proc::Runner>,
}

thread_local! {
    /// Per package root: what its dependencies were last built to, and the
    /// digest of the sources that produced them. Not a `static`, because a
    /// compiled package is not something to share between threads -- and the
    /// server asks from one.
    static BUILT: RefCell<HashMap<PathBuf, (u64, Rc<Built>)>> = RefCell::new(HashMap::new());
}

/// Everything `graph`'s root depends on, compiled -- from memory when nothing
/// they are made of has changed since last time.
fn dependencies(root: &std::path::Path, graph: &crate::package::PackageGraph) -> Rc<Built> {
    let digest = sources_digest(graph);
    if let Some(had) = BUILT.with(|b| {
        b.borrow()
            .get(root)
            .filter(|(d, _)| *d == digest)
            .map(|(_, built)| built.clone())
    }) {
        return had;
    }
    let opts = crate::Options::debug();
    let cache = crate::incremental::Cache::new(root, opts);
    let compiled = crate::pipeline::compile_graph(graph, opts, cache.as_ref());
    let me = graph.root();
    let deps: Vec<_> = graph.packages[me]
        .deps
        .iter()
        .zip(&graph.packages[me].dep_names)
        .filter_map(|(id, alias)| {
            compiled.packages[*id]
                .as_ref()
                .map(|p| (*alias, Rc::new(p.clone())))
        })
        .collect();
    // A macro could live in any of them, or in the standard library they were
    // compiled against.
    let mut held: Vec<meadow_compiler::CompiledPackage> = compiled.std.clone();
    held.extend(compiled.packages.iter().flatten().cloned());
    // What the package's macro calls answered, where a build of it keeps them
    // and under the fingerprint it keeps them under: an editor opening a
    // package that was built answers them without running anything.
    let mut procs = crate::proc::Macros::owning(held);
    if let Some(cache) = &cache {
        let deps: Vec<u64> = graph.packages[me]
            .deps
            .iter()
            .map(|&d| compiled.fingerprints[d])
            .collect();
        procs = procs.remembering(
            cache.macros(&graph.packages[me].name),
            crate::incremental::macros_print(&deps),
        );
    }
    let built = Rc::new(Built {
        deps,
        procs: Rc::new(procs),
    });
    BUILT.with(|b| {
        b.borrow_mut()
            .insert(root.to_path_buf(), (digest, built.clone()))
    });
    built
}

/// What every package in `graph` but the root is made of, as one number: the
/// name, length and last change of each of their files. Enough to notice a
/// dependency being edited, and cheap enough to ask on every keystroke.
fn sources_digest(graph: &crate::package::PackageGraph) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    let me = graph.root();
    for (id, pkg) in graph.packages.iter().enumerate() {
        if id == me {
            continue;
        }
        for m in &pkg.modules {
            if let meadow_compiler::source::SourceKind::File(name) = m.source.kind {
                let path = name.to_string();
                feed(path.as_bytes());
                if let Ok(meta) = std::fs::metadata(&path) {
                    feed(&meta.len().to_le_bytes());
                    if let Ok(t) = meta.modified()
                        && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
                    {
                        feed(&d.as_nanos().to_le_bytes());
                    }
                }
            }
        }
    }
    h
}

/// Whether the language server may fetch what a package depends on, as a
/// build would: `meadow lsp --fetch`, which the VS Code extension passes in a
/// workspace the user trusts. A manifest is text anyone may have written, and
/// what it names is cloned, compiled, and has its macros run -- so in a
/// workspace nobody vouched for, opening a file fetches nothing, and only
/// what a build already fetched is used.
static FETCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The packages the server has already fetched for, or tried to: once each,
/// so that one that cannot be fetched is not tried again on every edit.
static FETCHED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Let the server fetch dependencies (see [`FETCH`]).
pub fn allow_fetching() {
    FETCH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn fetching() -> bool {
    FETCH.load(std::sync::atomic::Ordering::Relaxed)
}

fn first_fetch(root: &std::path::Path) -> bool {
    let mut fetched = FETCHED.lock().unwrap_or_else(|e| e.into_inner());
    if fetched.iter().any(|r| r == root) {
        return false;
    }
    fetched.push(root.to_path_buf());
    true
}

/// Resolve `root`'s dependencies as a build does -- fetching what is not
/// cached -- and write the lockfile, as a build does, so that the next build
/// uses what the editor pinned.
fn fetch(root: &std::path::Path) -> Result<crate::package::PackageGraph, String> {
    let mut resolver = crate::package::Resolver::for_entry(root);
    let graph =
        crate::package::PackageGraph::build_all_with(&[root], &mut resolver).map_err(|d| d.msg)?;
    if !resolver.seen.is_empty() {
        resolver.lock.retain(&resolver.seen);
        let _ = resolver.lock.save(&crate::lock::dir_for(root));
    }
    Ok(graph)
}

/// [`load_package`], saying why when the file is in a package that cannot be
/// loaded. This is the one the server is given, so the reason reaches the editor.
pub fn find_package(
    file: &std::path::Path,
) -> Option<Result<meadow_lsp::analysis::PackageSources, String>> {
    use meadow_compiler::source::SourceKind;
    let root = crate::package::enclosing_root(file)?;
    // What is already in the cache, first: this runs on every edit, and only
    // the first load of a package should ever wait on the network.
    let mut resolver = crate::package::Resolver::for_entry(&root).offline();
    let graph = match crate::package::PackageGraph::build_all_with(&[&root], &mut resolver) {
        Ok(g) => g,
        Err(d) if d.msg.contains("is not in the cache") => {
            if fetching() && first_fetch(&root) {
                match fetch(&root) {
                    Ok(g) => g,
                    Err(why) => {
                        return Some(Err(format!(
                            "could not fetch the dependencies of `{}`: {why}",
                            root.display()
                        )));
                    }
                }
            } else {
                // The resolver's own words are about `--offline`, which nobody
                // here passed: say what is missing, and what fetches it.
                let what = d
                    .msg
                    .split('`')
                    .nth(1)
                    .unwrap_or("a dependency")
                    .to_string();
                return Some(Err(format!(
                    "`{what}` has not been fetched yet. The editor fetches a package's \
                     dependencies only in a workspace you trust -- trust this one, or run \
                     `meadow build` in `{}`, and it picks them up as soon as they are there.",
                    root.display()
                )));
            }
        }
        Err(d) => return Some(Err(d.msg)),
    };
    let pkg = &graph.packages[graph.root()];
    let modules = pkg
        .modules
        .iter()
        .map(|m| meadow_lsp::analysis::ModuleFile {
            path: m.path.clone(),
            name: m.name,
            // Canonical, because the other side of the comparison is a path
            // an editor chose the spelling of.
            file: match m.source.kind {
                SourceKind::File(name) => {
                    crate::package::canonical(&PathBuf::from(name.to_string()))
                }
                SourceKind::Interactive => PathBuf::new(),
            },
            text: m.source.content.to_string(),
        })
        .collect();
    let built = dependencies(&root, &graph);
    Some(Ok(meadow_lsp::analysis::PackageSources {
        name: pkg.name,
        root: root.clone(),
        modules,
        deps: built.deps.clone(),
        procs: Some(built.procs.clone()),
    }))
}
