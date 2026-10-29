//! The unit of compilation is a **package**: a directory of modules plus a list of
//! dependency packages. Modules inside one package may refer to each other freely
//! (mutual recursion is fine); packages form a DAG and may not — a dependency cycle
//! is a hard error, exactly like cargo crates.
//!
//! Discovery is filesystem-based. A package root optionally carries a
//! `Meadow.toml` manifest, in a Cargo-like format:
//!
//! ```toml
//! [package]
//! name = "demo"
//! version = "0.1.0"
//!
//! [dependencies]
//! util = "../util"                 # bare string = path
//! shared = { path = "../shared" }  # inline table with `path`
//! ```
//!
//! `[package]` name/version may also be given at the top level, and the legacy
//! file name `Meadow.pkg` is still accepted. Without a manifest the directory (or
//! single `.mw` file) is a standalone package named after its stem. The embedded
//! `Std` package (see [`crate::stdlib`]) is always an implicit dependency and
//! never needs to be listed.
//!
//! A manifest may also configure the build profiles, which is how a package
//! changes what `--debug` and `--release` mean for it:
//!
//! ```toml
//! [profile.debug]
//! opt-level = 1            # 0, 1, 2 — or "O1"
//!
//! [profile.release]
//! opt-level = 2
//! strictness = "strict"    # "lenient" | "strict"
//! runtime = "glade"        # "glade" | "silo"
//! backend = "aot"          # Glade's: "jit" | "aot" (or "vm"); or `aot = true`
//! prune = true             # compile only what `main` reaches
//! cfg = "fast, feature=gpu" # flags `@cfg(…)` can test
//! threads = 4              # OS threads for the green ones: `-j`
//! nursery = "8M"           # how large Glade's nursery may grow: `--nursery`
//! cycles = "trial"         # how Silo finds cycles: "tracing" (the default) | "trial"
//! leaks = true             # report what the program left behind: `--leaks`
//! target = "aarch64"       # the processor a native build is for: `--target`
//! ```
//!
//! `runtime` picks the runtime system, and the two are two backends of their
//! own. **Glade**, the default, has a choice of its own: `backend = "jit"`,
//! bytecode compiled as it runs, or `"aot"`, compiled ahead of time into an
//! executable (`"vm"`, the interpreter alone, is there too). **Silo** has
//! none: it is compiled all the way down by LLVM, ahead of time by what it is,
//! and a `backend` beside it is warned about and ignored.
//!
//! Only the keys that are present are overridden; the rest keep the profile's
//! built-in meaning (see [`crate::Profile`]). A command-line flag wins over
//! both.
//!
//! Several packages can share one root manifest as a **workspace** -- one
//! `target`, one set of profiles, versions and dependencies written once. A
//! member takes those with `version.workspace = true` and
//! `util = { workspace = true }`; see [`crate::workspace`].

use meadow_compiler::{
    OptLevel, Options, Strictness,
    diagnostics::Diagnostic,
    intern::InternedString,
    source::{Source, SourceKind},
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub type PackageId = usize;

#[derive(Debug, Clone)]
pub struct ModuleSource {
    /// Dotted path from the package's source root; empty for the root module.
    pub path: Vec<InternedString>,
    pub name: InternedString,
    pub source: Source,
}

/// Which commit of a git dependency to build.
///
/// Only [`GitRef::Rev`] names one outright. A branch moves, and a tag *can* be
/// moved, which is why what was resolved is written to the lockfile rather than
/// worked out afresh on every build.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GitRef {
    /// Whatever the remote's `HEAD` points at -- its default branch.
    #[default]
    Default,
    Branch(String),
    Tag(String),
    /// A commit, which cannot mean anything else later.
    Rev(String),
    /// `version = "1.2.0"`: the newest release the repository has tagged that
    /// does not break that one. Which release that is depends on what else the
    /// build wants, so it is decided while resolving rather than here.
    Version(crate::semver::Req),
}

impl GitRef {
    /// How this reads in a manifest and a lock entry: `branch=main`, `tag=v1`.
    pub fn written(&self) -> Option<String> {
        match self {
            GitRef::Default => None,
            GitRef::Branch(b) => Some(format!("branch={b}")),
            GitRef::Tag(t) => Some(format!("tag={t}")),
            GitRef::Rev(r) => Some(format!("rev={r}")),
            // The requirement, not the release it resolved to: this is how a
            // lock entry is found again, and a resolution that moved within
            // the requirement must still find the entry it is replacing.
            GitRef::Version(req) => Some(format!("version={req}")),
        }
    }

    /// What to ask `git` to fetch.
    pub fn refspec(&self) -> &str {
        match self {
            GitRef::Default => "HEAD",
            GitRef::Branch(b) => b,
            GitRef::Tag(t) => t,
            GitRef::Rev(r) => r,
            // A requirement is resolved to a tag before anything is fetched
            // for it; `refspec` is never reached with one.
            GitRef::Version(_) => "HEAD",
        }
    }
}

/// Where a dependency's source is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepSource {
    /// `util = { path = "../util" }` -- a directory, relative to the manifest,
    /// or absolute for one inherited from a workspace.
    Path(PathBuf),
    /// `json = { git = "https://github.com/…", tag = "v1.2.0" }`, and
    /// `subdir = "lib/json"` where the package is a directory of the
    /// repository rather than all of it.
    Git {
        url: String,
        reference: GitRef,
        subdir: Option<String>,
    },
}

/// One entry of `[dependencies]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub source: DepSource,
    /// `features = ["gpu"]`: the features of it this package turns on.
    pub features: Vec<String>,
    /// `default-features = false` turns its `default` ones off.
    pub default_features: bool,
}

/// The features a build was asked for, of the packages it was pointed at:
/// `--features a,b`, `--no-default-features`, `--all-features`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskedFeatures {
    pub named: Vec<String>,
    pub no_default: bool,
    pub all: bool,
}

static ASKED: std::sync::RwLock<Option<AskedFeatures>> = std::sync::RwLock::new(None);

/// Say what the command line asked for, before anything is built.
pub fn ask_features(asked: AskedFeatures) {
    if let Ok(mut slot) = ASKED.write() {
        *slot = Some(asked);
    }
}

fn asked_features() -> AskedFeatures {
    ASKED
        .read()
        .ok()
        .and_then(|a| a.clone())
        .unwrap_or_default()
}

/// `have` with the flags that say `features` are on, as `--cfg` writes
/// them: `feature=gpu`, which is what `@cfg(feature = "gpu")` asks.
pub fn feature_flags(have: Option<InternedString>, features: &[String]) -> Option<InternedString> {
    if features.is_empty() {
        return have;
    }
    let mut all: Vec<String> = have.map(|h| h.to_string()).into_iter().collect();
    all.extend(features.iter().map(|f| format!("feature={f}")));
    Some(InternedString::from(all.join(", ")))
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// `[features]`: the features the package offers, each with the others
    /// it turns on with it. `default` is the ones on unless a build says not.
    pub feature_table: Vec<(String, Vec<String>)>,
    /// Each dependency, in the order it was written.
    pub deps: Vec<Dependency>,
    /// What is wrong with the manifest but not wrong enough to stop the build:
    /// a spelling on its way out, say. Printed once, by whoever built it.
    pub warnings: Vec<String>,
    /// `[profile.<name>]` sections, keyed by profile name.
    pub profiles: HashMap<String, ProfileConfig>,
    /// Whether the manifest describes a package. Only a workspace's root
    /// manifest can say no: one with a `[workspace]` and no `[package]` is
    /// *virtual*, the members' and nobody else's.
    pub is_package: bool,
    /// The `[workspace]` sections, when this is a workspace's root.
    pub workspace: Option<WorkspaceManifest>,
    /// What is wrong with it, beyond what a line-based reader skips: something
    /// inherited from a workspace that does not have it.
    pub problems: Vec<String>,
}

/// What a workspace's root manifest says about the workspace -- see
/// [`crate::workspace`].
#[derive(Debug, Clone, Default)]
pub struct WorkspaceManifest {
    /// `members = ["app", "libs/*"]`: directories, relative to the root, where
    /// `*` and `?` match within one path segment.
    pub members: Vec<String>,
    /// Directories under the root that are not members, even when a pattern
    /// or a path dependency would make them one.
    pub exclude: Vec<String>,
    /// What a command run at the root means when no package is named.
    pub default_members: Vec<String>,
    /// `[workspace.package]` `version`, for `version.workspace = true`.
    pub version: Option<String>,
    /// `[workspace.dependencies]`, for `name = { workspace = true }`. A path
    /// in one is relative to the workspace root, not to the member.
    pub deps: Vec<Dependency>,
}

/// What one `[profile.<name>]` section says.
///
/// Every field is optional, and absent means "whatever that profile already
/// meant" — a manifest that only wants a different optimization level in debug
/// builds says exactly that and inherits the rest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProfileConfig {
    pub opt: Option<OptLevel>,
    pub strictness: Option<Strictness>,
    /// How the program runs -- see [`crate::profile::Backend`].
    pub backend: Option<crate::profile::Backend>,
    /// Whether to compile only what the entry point reaches -- see
    /// [`crate::profile::Resolved::prune`].
    pub prune: Option<bool>,
    /// Flags for `@cfg(…)` to test, comma-separated: `cfg = "fast, feature=gpu"`.
    /// Added to whatever the layer below turned on.
    pub cfg: Option<InternedString>,
    /// `profile = true`: build so this can be profiled, and sample it when it
    /// runs -- see [`crate::profile::Resolved::sample`]. Carries the debug info
    /// a profile's frames are named by, which an ordinary release build drops.
    pub profile: Option<bool>,
    /// The runtime system: Glade or Silo -- see [`crate::aot::Runtime`].
    pub runtime: Option<crate::aot::Runtime>,
    /// `threads = N`: how many OS threads run the green ones, as `-j` says.
    pub threads: Option<usize>,
    /// `nursery = "8M"`: how large Glade's collector lets a thread's nursery
    /// grow, in bytes, as `--nursery` says. One that is not a size, or is
    /// outside what a nursery may be, is ignored.
    pub nursery: Option<u64>,
    /// `leaks = true`: report what the program left behind, as `--leaks`.
    pub leaks: Option<bool>,
    /// `target = "aarch64"`: the processor a native build is for, as
    /// `--target` says.
    pub target: Option<InternedString>,
    /// `cycles = "trial"`: how a Silo program that can tie a knot has its
    /// cycles found -- concurrent tracing, the default, or trial deletion
    /// (`docs/SILO.md`, "Cycles"). `MEADOW_CYCLES` says the same, and wins.
    pub cycles: Option<meadow_llvm::Cycles>,
}

impl ProfileConfig {
    /// This section applied on top of `base`.
    pub fn apply(self, base: Options) -> Options {
        Options {
            opt: self.opt.unwrap_or(base.opt),
            strictness: self.strictness.unwrap_or(base.strictness),
            // A profile's frames are named by the debug info, so asking to
            // profile is asking to keep it.
            debug_info: base.debug_info || self.profile.unwrap_or(false),
            entry_name: base.entry_name,
            cfg: match self.cfg {
                Some(flags) => base.cfg.with_flags(&flags),
                None => base.cfg,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct Package {
    pub id: PackageId,
    pub name: InternedString,
    /// The manifest's `version`. A lone file has no manifest, and so no
    /// version to report.
    pub version: Option<String>,
    /// The manifest's `[features]`: see [`Manifest::feature_table`].
    pub feature_table: Vec<(String, Vec<String>)>,
    /// What this package asks of each dependency's features, parallel to
    /// `deps`: the ones it names, and whether it takes the defaults.
    pub dep_features: Vec<(Vec<String>, bool)>,
    /// Where the package came from, as a build reports it: its directory, or
    /// for a git dependency the repository and commit.
    pub origin: String,
    pub root: PathBuf,
    pub modules: Vec<ModuleSource>,
    pub deps: Vec<PackageId>,
    /// What this package calls each of its dependencies -- the key in its
    /// `[dependencies]`, which is not always what the dependency calls itself.
    /// Parallel to `deps`.
    pub dep_names: Vec<InternedString>,
}

impl Package {
    /// Who the package is, for naming the types and effects it declares.
    ///
    /// Two copies of one package at different versions are different packages,
    /// and their types are different types, so the version is part of who it
    /// is. A package with no manifest has only its name.
    pub fn ident(&self) -> String {
        match &self.version {
            Some(v) => format!("{}@{v}", self.name),
            None => self.name.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackageGraph {
    pub packages: Vec<Package>,
    /// Topological order: every package appears after all of its dependencies.
    order: Vec<PackageId>,
    /// The packages asked for, in the order they were asked for.
    roots: Vec<PackageId>,
    /// What the manifests read along the way had to say that did not stop the
    /// build. Each is reported once, however many packages share a manifest.
    warnings: Vec<String>,
}

impl PackageGraph {
    /// The features each package is built with, by its place in `packages`.
    ///
    /// A package asked for by the build has what the command line names, and
    /// its `default` ones unless it says not; a dependency has what each
    /// package that depends on it names, and its defaults unless every one of
    /// them declines them. A feature turns on the ones its entry in
    /// `[features]` lists. One no manifest declares is an error that says
    /// which there are.
    pub fn enabled_features(&self) -> Result<Vec<Vec<String>>, String> {
        let asked = asked_features();
        let n = self.packages.len();
        let mut named: Vec<Vec<String>> = vec![Vec::new(); n];
        let mut defaults = vec![false; n];
        let mut wanted_by: Vec<Vec<String>> = vec![Vec::new(); n];
        for &r in &self.roots {
            let pkg = &self.packages[r];
            if asked.all {
                named[r].extend(pkg.feature_table.iter().map(|(f, _)| f.clone()));
            }
            named[r].extend(asked.named.iter().cloned());
            defaults[r] = !asked.no_default;
            wanted_by[r].push("the command line".to_string());
        }
        // Dependents before what they depend on, so that all that is asked
        // of a package is known before it is closed.
        let mut enabled: Vec<Vec<String>> = vec![Vec::new(); n];
        for &pid in self.order.iter().rev() {
            let pkg = &self.packages[pid];
            let known = |f: &str| pkg.feature_table.iter().any(|(k, _)| k == f);
            let mut on: Vec<String> = Vec::new();
            let mut todo: Vec<String> = named[pid].clone();
            if defaults[pid] && known("default") {
                todo.push("default".to_string());
            }
            while let Some(f) = todo.pop() {
                if on.contains(&f) {
                    continue;
                }
                let Some((_, more)) = pkg.feature_table.iter().find(|(k, _)| *k == f) else {
                    let have: Vec<String> = pkg
                        .feature_table
                        .iter()
                        .filter(|(k, _)| k != "default")
                        .map(|(k, _)| format!("`{k}`"))
                        .collect();
                    return Err(format!(
                        "`{}` has no feature `{f}`, which {} asks for: {}",
                        pkg.name,
                        wanted_by[pid].join(" and "),
                        if have.is_empty() {
                            "its Meadow.toml has no `[features]`".to_string()
                        } else {
                            format!("it has {}", have.join(", "))
                        }
                    ));
                };
                todo.extend(more.iter().cloned());
                on.push(f);
            }
            on.retain(|f| f != "default");
            on.sort();
            for ((dep, (features, takes_defaults)), alias) in
                pkg.deps.iter().zip(&pkg.dep_features).zip(&pkg.dep_names)
            {
                let _ = alias;
                named[*dep].extend(features.iter().cloned());
                defaults[*dep] |= *takes_defaults;
                if !features.is_empty() {
                    wanted_by[*dep].push(format!("`{}`", pkg.name));
                }
            }
            enabled[pid] = on;
        }
        Ok(enabled)
    }

    pub fn order(&self) -> &[PackageId] {
        &self.order
    }

    /// What reading the manifests had to say: a spelling on its way out, say.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The package asked for -- the first, if several were.
    pub fn root(&self) -> PackageId {
        self.roots[0]
    }

    /// Every package asked for, in the order they were asked for.
    pub fn roots(&self) -> &[PackageId] {
        &self.roots
    }

    /// `root` and everything it depends on, dependencies first.
    pub fn closure(&self, root: PackageId) -> Vec<PackageId> {
        let mut wanted = vec![false; self.packages.len()];
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !std::mem::replace(&mut wanted[id], true) {
                stack.extend(&self.packages[id].deps);
            }
        }
        self.order
            .iter()
            .copied()
            .filter(|&id| wanted[id])
            .collect()
    }

    /// Discover the package rooted at `entry` and everything it depends on.
    pub fn build(entry: &Path) -> Result<PackageGraph, Diagnostic> {
        PackageGraph::build_all(&[entry])
    }

    /// Discover several packages and everything they depend on, each package
    /// once however many of them depend on it -- a workspace's members.
    pub fn build_all(entries: &[&Path]) -> Result<PackageGraph, Diagnostic> {
        let mut resolver = Resolver::for_entry(entries.first().copied().unwrap_or(Path::new(".")));
        PackageGraph::build_all_with(entries, &mut resolver)
    }

    /// The same, with control over how dependencies are fetched and what is
    /// pinned. Whoever passes the resolver owns writing the lockfile back.
    pub fn build_all_with(
        entries: &[&Path],
        resolver: &mut Resolver,
    ) -> Result<PackageGraph, Diagnostic> {
        // Resolving a version requirement can raise a release something
        // earlier in the walk already resolved against. When it does, the walk
        // is made again, now that the decision is known -- so that one copy of
        // the package serves everything that can share it. Each pass raises at
        // least one release, so this settles.
        for _ in 0..8 {
            resolver.reresolve = false;
            let graph = PackageGraph::walk(entries, resolver)?;
            if !resolver.reresolve {
                return Ok(graph);
            }
        }
        PackageGraph::walk(entries, resolver)
    }

    fn walk(entries: &[&Path], resolver: &mut Resolver) -> Result<PackageGraph, Diagnostic> {
        let mut builder = Builder {
            resolver,
            packages: Vec::new(),
            by_root: HashMap::new(),
            order: Vec::new(),
            stack: Vec::new(),
            warnings: Vec::new(),
        };
        let mut roots = Vec::new();
        for entry in entries {
            let id = builder.visit(entry)?;
            if !roots.contains(&id) {
                roots.push(id);
            }
        }
        assert!(!roots.is_empty(), "a package graph needs a package");
        Ok(PackageGraph {
            packages: builder.packages,
            order: builder.order,
            roots,
            warnings: builder.warnings,
        })
    }
}

/// How a dependency becomes a directory.
///
/// A path dependency already is one. A git dependency is fetched, at the commit
/// [`crate::lock`] pinned when there is one -- which is what makes a second
/// build of an unchanged project touch the network not at all.
pub struct Resolver {
    pub lock: crate::lock::Lock,
    pub net: crate::git::Net,
    /// Refuse anything that would change the lockfile. What CI wants: a build
    /// that quietly re-pins is a build of something nobody reviewed.
    pub locked: bool,
    /// Ignore what is pinned and take what the manifest's reference names now.
    /// `meadow update`.
    pub update: bool,
    /// With `update`, only these dependencies by name. Empty means all of them,
    /// which is what `meadow update` with no names does.
    pub only: Vec<String>,
    /// What was resolved, so that entries nothing wants any more can be
    /// dropped when the lockfile is written.
    pub seen: Vec<(String, String)>,
    /// Where fetched repositories are kept. Held here rather than looked up,
    /// so a test can point it somewhere of its own.
    pub cache: PathBuf,
    /// For each checkout a git dependency resolved to, how a build should name
    /// where it came from: `https://…#a1b2c3d4`.
    pub origins: HashMap<PathBuf, String>,
    /// Which release each `version = "…"` dependency resolved to, by
    /// repository and the digit that breaks compatibility.
    ///
    /// Everything in a build that can share one copy of a package does: two
    /// packages wanting `1.2` and `1.4` get `1.4`, and only a requirement that
    /// *cannot* be met alongside another -- `2.0` beside `1.4` -- becomes a
    /// second copy.
    pub releases: HashMap<(String, (u64, u64)), crate::semver::Version>,
    /// Set when a decision already made had to be raised, so that whatever the
    /// earlier requirement resolved to is resolved again.
    pub reresolve: bool,
}

/// How a run resolves dependencies: `--offline`, `--locked`, and whether this
/// is an update.
///
/// A property of the invocation rather than of any one package, so it is set
/// once from the command line before anything is built, and read wherever a
/// dependency is resolved. Threading it through every build entry point would
/// say the same thing in more places.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    pub net: crate::git::Net,
    pub locked: bool,
    pub update: bool,
}

static POLICY: std::sync::OnceLock<Policy> = std::sync::OnceLock::new();

/// Say how this run resolves. Only the first call counts, which is the one the
/// command line makes before any work starts.
pub fn set_policy(p: Policy) {
    let _ = POLICY.set(p);
}

pub fn policy() -> Policy {
    POLICY.get().copied().unwrap_or_default()
}

impl Resolver {
    /// A resolver that reads the lockfile for a build rooted at `entry` and may
    /// fetch. It does not write: saving is the caller's, once the whole graph
    /// has resolved.
    pub fn for_entry(entry: &Path) -> Resolver {
        let p = policy();
        Resolver {
            lock: crate::lock::Lock::load(&crate::lock::dir_for(entry)),
            net: p.net,
            locked: p.locked,
            update: p.update,
            only: Vec::new(),
            seen: Vec::new(),
            cache: crate::git::default_cache().unwrap_or_default(),
            origins: HashMap::new(),
            releases: HashMap::new(),
            reresolve: false,
        }
    }

    /// This resolver, never fetching whatever this run's policy says -- for
    /// the editor, which reads a package because a file in it was opened, not
    /// because anyone asked for a build. What is already cached is used.
    pub fn offline(mut self) -> Resolver {
        self.net = crate::git::Net::Offline;
        self
    }

    /// Where `dep`, written in the manifest in `from`, can be read.
    /// Which release of `url` meets `req`, given what the rest of the build
    /// already settled on.
    ///
    /// The newest release that meets every requirement in its compatibility
    /// group wins. Raising a group's release after something resolved against
    /// the old one asks for the graph to be built again, so that everything
    /// ends up on the one copy.
    fn release_for(
        &mut self,
        name: &str,
        url: &str,
        req: &crate::semver::Req,
    ) -> Result<(String, crate::semver::Version), String> {
        let group = (url.to_string(), req.least.breaking());
        if let Some(chosen) = self.releases.get(&group)
            && req.allows(chosen)
        {
            let chosen = chosen.clone();
            return Ok((self.tag_of(url, &chosen), chosen));
        }
        let have = crate::git::releases(&self.cache, url, self.net)?;
        let versions: Vec<crate::semver::Version> = have.iter().map(|(v, _)| v.clone()).collect();
        // A group's requirements are met together or not at all: the newest
        // release that meets this one must also meet what was decided before.
        let want = match self.releases.get(&group) {
            Some(earlier) => req.strictest(&crate::semver::Req {
                least: earlier.clone(),
            }),
            None => req.clone(),
        };
        let Some(best) = want.best(&versions) else {
            let listed: Vec<String> = have
                .iter()
                .rev()
                .take(5)
                .map(|(v, _)| v.to_string())
                .collect();
            return Err(if listed.is_empty() {
                format!(
                    "`{name}` at {url} has no releases: nothing there is tagged `v1.2.3`.\n                     Depend on a branch instead -- `{{ git = \"{url}\" }}` -- or tag a release."
                )
            } else {
                format!(
                    "no release of `{name}` at {url} is {want}; it has {}",
                    listed.join(", ")
                )
            });
        };
        let best = best.clone();
        if self.releases.insert(group, best.clone()).is_some() {
            self.reresolve = true;
        }
        Ok((self.tag_of(url, &best), best))
    }

    /// The tag a release was written as: `v1.2.0` or `1.2.0`, as the
    /// repository spells it.
    fn tag_of(&self, url: &str, version: &crate::semver::Version) -> String {
        crate::git::releases(&self.cache, url, crate::git::Net::Offline)
            .ok()
            .and_then(|have| {
                have.into_iter()
                    .find(|(v, _)| v == version)
                    .map(|(_, tag)| tag)
            })
            .unwrap_or_else(|| format!("v{version}"))
    }

    fn resolve(&mut self, dep: &Dependency, from: &Path) -> Result<PathBuf, String> {
        let DepSource::Git {
            url,
            reference,
            subdir,
        } = &dep.source
        else {
            let DepSource::Path(rel) = &dep.source else {
                unreachable!("a dependency is a path or a git repository")
            };
            return Ok(from.join(rel));
        };
        // A requirement is not a reference until it is decided which release
        // meets it -- here, and once for everything in the build that can
        // share the release.
        let source = crate::lock::source_id(&dep.source).expect("a git source has an id");
        // Updating means ignoring what was pinned, so that the reference is
        // looked at afresh. Naming dependencies narrows that to those: the
        // rest keep the commits they had, which is the point of updating one
        // thing rather than everything.
        let refresh = self.update && (self.only.is_empty() || self.only.contains(&dep.name));
        let resolved;
        let mut resolved_version = None;
        let reference = match reference {
            GitRef::Version(req) => {
                // The release the lock names is the release, while it still
                // meets the requirement: one tagged since is taken by
                // `meadow update`, not by a build on a machine that has not
                // fetched this yet -- where the newest release's commit would
                // otherwise be held against the one pinned, as a tag that
                // moved.
                let locked = if refresh {
                    None
                } else {
                    self.lock
                        .find(&dep.name, &source)
                        .and_then(|l| l.version.as_deref())
                        .and_then(crate::semver::Version::parse)
                        .filter(|v| req.allows(v))
                };
                let (tag, version) = match locked {
                    Some(version) => {
                        self.releases
                            .entry((url.to_string(), req.least.breaking()))
                            .or_insert_with(|| version.clone());
                        (self.tag_of(url, &version), version)
                    }
                    None => self.release_for(&dep.name, url, req)?,
                };
                resolved_version = Some(version.to_string());
                resolved = GitRef::Tag(tag);
                &resolved
            }
            other => other,
        };
        let pinned = if refresh {
            None
        } else {
            self.lock.find(&dep.name, &source).map(|l| l.rev.clone())
        };
        if self.locked && pinned.is_none() {
            return Err(crate::lock::changed(&format!(
                "`{}` from {}",
                dep.name,
                crate::lock::describes(&source)
            )));
        }
        let got = crate::git::ensure(&self.cache, url, reference, pinned.as_deref(), self.net)?;
        // The package is the directory `subdir` says, where it says one.
        let at = match subdir {
            Some(d) => got.path.join(d),
            None => got.path.clone(),
        };
        if !at.join("Meadow.toml").is_file() {
            return Err(format!(
                "`{}` from {} has no `Meadow.toml`{}",
                dep.name,
                url,
                match subdir {
                    Some(d) => format!(" in `{d}`"),
                    None => " at its root: name the directory it is in with `subdir`".to_string(),
                }
            ));
        }
        self.origins.insert(
            canonical(&at),
            format!("{url}#{}", &got.rev[..8.min(got.rev.len())]),
        );
        self.seen.push((dep.name.clone(), source.clone()));
        self.lock.insert(crate::lock::Locked {
            name: dep.name.clone(),
            source,
            rev: got.rev,
            tree: got.tree,
            version: resolved_version,
        });
        Ok(at)
    }
}

struct Builder<'r> {
    resolver: &'r mut Resolver,
    packages: Vec<Package>,
    by_root: HashMap<PathBuf, PackageId>,
    order: Vec<PackageId>,
    /// Roots currently being visited, for cycle reporting.
    stack: Vec<(PathBuf, InternedString)>,
    warnings: Vec<String>,
}

impl Builder<'_> {
    fn visit(&mut self, path: &Path) -> Result<PackageId, Diagnostic> {
        let canon = canonical(path);
        if let Some(id) = self.by_root.get(&canon) {
            return Ok(*id);
        }
        if let Some(pos) = self.stack.iter().position(|(p, _)| *p == canon) {
            let chain = self
                .stack
                .iter()
                .skip(pos)
                .map(|(_, n)| n.to_string())
                .chain(std::iter::once(package_name(&canon).to_string()))
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(Diagnostic {
                msg: format!("packages cannot be mutually recursive: {chain}"),
                filename: canon.display().to_string(),
                label: (
                    "dependency cycle starts here".to_string(),
                    Default::default(),
                ),
                extra_labels: vec![],
            });
        }

        let manifest = Manifest::load(&canon).map_err(|e| io_diag(&canon, e))?;
        if let Some(m) = &manifest {
            let problem = if !m.is_package {
                Some(format!(
                    "{} is a workspace with no package of its own: build its members \
                     with `--workspace`, or one of them with `-p NAME`",
                    crate::workspace::shown(&canon)
                ))
            } else {
                (!m.problems.is_empty()).then(|| m.problems.join("; "))
            };
            if let Some(msg) = problem {
                return Err(Diagnostic {
                    msg,
                    filename: crate::workspace::shown(&canon.join("Meadow.toml")),
                    label: ("here".to_string(), Default::default()),
                    extra_labels: vec![],
                });
            }
        }
        if let Some(m) = &manifest {
            let where_ = crate::workspace::shown(&canon.join("Meadow.toml"));
            for w in &m.warnings {
                let said = format!("{where_}: {w}");
                if !self.warnings.contains(&said) {
                    self.warnings.push(said);
                }
            }
        }
        let name = manifest
            .as_ref()
            .map(|m| InternedString::from(m.name.as_str()))
            .unwrap_or_else(|| package_name(&canon));

        self.stack.push((canon.clone(), name));

        // resolve dependencies first so `order` ends up topologically sorted
        let mut dep_ids = Vec::new();
        let mut dep_names: Vec<InternedString> = Vec::new();
        let mut dep_features: Vec<(Vec<String>, bool)> = Vec::new();
        if let Some(m) = &manifest {
            for dep in &m.deps {
                let dep_path = match self.resolver.resolve(dep, &canon) {
                    Ok(p) => p,
                    Err(msg) => {
                        self.stack.pop();
                        return Err(Diagnostic {
                            msg: format!("dependency `{}` of `{name}`: {msg}", dep.name),
                            filename: crate::workspace::shown(&canon.join("Meadow.toml")),
                            label: ("declared here".to_string(), Default::default()),
                            extra_labels: vec![],
                        });
                    }
                };
                if !dep_path.exists() {
                    self.stack.pop();
                    return Err(Diagnostic {
                        msg: format!(
                            "dependency `{}` of `{name}` not found at {}",
                            dep.name,
                            dep_path.display()
                        ),
                        filename: canon.display().to_string(),
                        label: ("declared here".to_string(), Default::default()),
                        extra_labels: vec![],
                    });
                }
                dep_ids.push(self.visit(&dep_path)?);
                dep_names.push(InternedString::from(dep.name.as_str()));
                dep_features.push((dep.features.clone(), dep.default_features));
            }
        }

        self.stack.pop();

        let modules = discover_modules(&canon, name)?;
        let id = self.packages.len();
        let version = manifest.as_ref().map(|m| m.version.clone());
        let feature_table = manifest
            .as_ref()
            .map(|m| m.feature_table.clone())
            .unwrap_or_default();
        let origin = self
            .resolver
            .origins
            .get(&canon)
            .cloned()
            .unwrap_or_else(|| crate::workspace::shown(&canon));
        self.packages.push(Package {
            id,
            name,
            version,
            feature_table,
            dep_features,
            origin,
            root: canon.clone(),
            modules,
            deps: dep_ids,
            dep_names,
        });
        self.by_root.insert(canon, id);
        self.order.push(id);
        Ok(id)
    }
}

/// Manifest file names, in precedence order.
const MANIFEST_NAMES: &[&str] = &["Meadow.toml", "Meadow.pkg"];

/// The manifest file in `dir`, whichever name it goes by.
pub fn manifest_path(dir: &Path) -> Option<PathBuf> {
    named(dir).exact
}

/// A manifest in `dir` whose name is spelled differently -- `Meadow.toml` for
/// `Meadow.toml`. Worth finding, because a file system that does not tell the
/// two apart will hand one over for the other, and one that does will say the
/// package is not a package at all.
pub fn misnamed_manifest(dir: &Path) -> Option<PathBuf> {
    named(dir).misspelled
}

struct Named {
    exact: Option<PathBuf>,
    misspelled: Option<PathBuf>,
}

/// What `dir` holds that could be a manifest, by how it is spelled.
///
/// The directory is read rather than a path asked about: `Path::is_file` goes
/// through the file system, and most of them answer `Meadow.toml` for a file
/// called `Meadow.toml`.
fn named(dir: &Path) -> Named {
    let mut out = Named {
        exact: None,
        misspelled: None,
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        for want in MANIFEST_NAMES {
            if name == *want {
                out.exact.get_or_insert_with(|| entry.path());
            } else if name.eq_ignore_ascii_case(want) {
                out.misspelled.get_or_insert_with(|| entry.path());
            }
        }
    }
    out
}

/// Whether `name` is a package name.
///
/// PascalCase, because a package is named where modules are: the first segment
/// of a `use` path is the package and every segment after it is a module, and
/// one rule for both is easier to hold than two.
pub fn is_package_name(name: &str) -> bool {
    let mut cs = name.chars();
    cs.next().is_some_and(|c| c.is_ascii_uppercase()) && cs.all(|c| c.is_ascii_alphanumeric())
}

/// `name` as a package name, for suggesting one: each run of letters and
/// digits capitalised, and everything else dropped. An initialism comes out as
/// a word -- `mini-ml` as `MiniMl` -- which a person may want to write
/// `MiniML` instead.
pub fn as_package_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut starting = true;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            starting = true;
            continue;
        }
        if starting {
            out.extend(c.to_uppercase());
            starting = false;
        } else {
            out.push(c);
        }
    }
    out
}

impl Manifest {
    pub fn load(dir: &Path) -> std::io::Result<Option<Manifest>> {
        let found = named(dir);
        let Some(file) = found.exact.clone().or_else(|| found.misspelled.clone()) else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(&file)?;
        let (mut manifest, inherits) = parse_manifest(&text, dir);
        if found.exact.is_none()
            && let Some(name) = file.file_name().and_then(|n| n.to_str())
        {
            manifest.problems.push(format!(
                "a package's manifest is `Meadow.toml`; this one is `{name}`"
            ));
        }
        if !is_package_name(&manifest.name.to_string()) {
            manifest.problems.push(format!(
                "`{}` is not a package name: a package is written where a module is, \
                 so it is `{}` rather than `{}`",
                manifest.name,
                as_package_name(&manifest.name.to_string()),
                manifest.name
            ));
        }
        if inherits.version || !inherits.deps.is_empty() {
            manifest.inherit(dir, inherits);
        }
        Ok(Some(manifest))
    }

    /// Fill in what `inherits` says comes from the workspace `dir` is in.
    fn inherit(&mut self, dir: &Path, inherits: Inherits) {
        let found = canonical(dir).ancestors().find_map(|a| {
            let (m, _) = parse_manifest(&read_manifest(a)?, a);
            m.workspace.map(|w| (a.to_path_buf(), w))
        });
        let Some((root, ws)) = found else {
            self.problems.push(format!(
                "`{}` inherits from a workspace, but is not in one: no `Meadow.toml` \
                 with a `[workspace]` above {}",
                self.name,
                crate::workspace::shown(dir)
            ));
            return;
        };
        let root_manifest = root.join("Meadow.toml");
        if inherits.version {
            match ws.version {
                Some(v) => self.version = v,
                None => self.problems.push(format!(
                    "`{}` says `version.workspace = true`, but {} has no `version` \
                     under `[workspace.package]`",
                    self.name,
                    crate::workspace::shown(&root_manifest)
                )),
            }
        }
        for name in inherits.deps {
            match ws.deps.iter().find(|d| d.name == name) {
                // A path is the root's to resolve; a git source means the same
                // thing wherever it is read from.
                Some(dep) => self.deps.push(Dependency {
                    name,
                    source: match &dep.source {
                        DepSource::Path(p) => DepSource::Path(root.join(p)),
                        git => git.clone(),
                    },
                    features: dep.features.clone(),
                    default_features: dep.default_features,
                }),
                None => self.problems.push(format!(
                    "`{}` depends on `{name}` from the workspace, but {} has no \
                     `{name}` under `[workspace.dependencies]`",
                    self.name,
                    crate::workspace::shown(&root_manifest)
                )),
            }
        }
    }

    /// The manifest governing `path`, which may be a package directory or a
    /// single `.mw` file inside one.
    ///
    /// Unreadable or missing is not an error here: a package need not have a
    /// manifest at all, and a build should not be stopped by one it could not
    /// read when everything it says is optional anyway.
    pub fn find(path: &Path) -> Option<Manifest> {
        let dir = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()?.to_path_buf()
        };
        Manifest::load(&dir).ok().flatten()
    }

    /// The `[profile.<name>]` section, or an empty one.
    pub fn profile(&self, name: &str) -> ProfileConfig {
        self.profiles.get(name).copied().unwrap_or_default()
    }
}

/// The package directory a file belongs to: the nearest ancestor holding a
/// `Meadow.toml`, or — for a package that has no manifest — the parent of the
/// `src` directory it sits under.
///
/// `None` when the file is not in a package at all, which is an ordinary thing
/// for an editor to be shown: a scratch file, or a `.mw` opened on its own.
pub fn enclosing_root(file: &Path) -> Option<PathBuf> {
    let mut dir = file.parent()?;
    loop {
        if manifest_path(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        // No manifest anywhere above: `src/` is the other thing that marks a
        // package root, and it is what `discover_modules` looks for.
        if dir.file_name().and_then(|n| n.to_str()) == Some("src")
            && dir.parent().is_some_and(|p| p.join("src").is_dir())
        {
            return dir.parent().map(Path::to_path_buf);
        }
        dir = dir.parent()?;
    }
}

/// The text of the manifest in `dir`, if it has one that can be read.
fn read_manifest(dir: &Path) -> Option<String> {
    MANIFEST_NAMES
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::read_to_string(p).ok())
}

/// What a manifest takes from its workspace, still to be looked up.
#[derive(Default)]
struct Inherits {
    /// `version.workspace = true`.
    version: bool,
    /// `name = { workspace = true }` or `name.workspace = true`.
    deps: Vec<String>,
}

/// A deliberately small line-based TOML reader — enough for `[package]` /
/// `[dependencies]` with string or `{ path = "…" }` values, the `[profile.*]`
/// and `[workspace*]` sections, and arrays of strings, which may run over
/// several lines.
fn parse_manifest(text: &str, dir: &Path) -> (Manifest, Inherits) {
    let mut name = package_name(dir).to_string();
    let mut version = "0.0.0".to_string();
    let mut deps: Vec<Dependency> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut feature_table: Vec<(String, Vec<String>)> = Vec::new();
    let mut profiles: HashMap<String, ProfileConfig> = HashMap::new();
    let mut section = String::new();
    let mut says_package = false;
    let mut workspace: Option<WorkspaceManifest> = None;
    let mut inherits = Inherits::default();

    for line in logical_lines(text) {
        let line = line.as_str();
        if let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = inner.trim().to_string();
            match section.as_str() {
                "package" => says_package = true,
                "workspace" | "workspace.package" | "workspace.dependencies" => {
                    workspace.get_or_insert_default();
                }
                _ => {}
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        // `key.workspace = true`, TOML's dotted spelling of
        // `key = { workspace = true }`.
        let from_workspace = |key: &str| {
            key.strip_suffix(".workspace")
                .map(str::trim)
                .filter(|_| unquote(value) == "true")
                .map(str::to_string)
        };

        match section.as_str() {
            "dependencies" => {
                if let Some(dep) = from_workspace(key) {
                    inherits.deps.push(dep);
                } else if inline_flag(value, "workspace") {
                    inherits.deps.push(key.to_string());
                } else if let Some(source) = dep_source(value, key, &mut |w| warnings.push(w)) {
                    let (features, default_features) = dep_features(value);
                    deps.push(Dependency {
                        name: key.to_string(),
                        source,
                        features,
                        default_features,
                    });
                }
            }
            "package" | "" => {
                if section.is_empty() && matches!(key, "name" | "version") {
                    says_package = true;
                }
                match key {
                    "name" => name = unquote(value).to_string(),
                    "version" if inline_flag(value, "workspace") => inherits.version = true,
                    "version" => version = unquote(value).to_string(),
                    // Where an unstable feature was asked for until 0.3:
                    // said, since what is written there now does nothing.
                    "features" => problems.push(
                        "`features` under `[package]` is not read any more: an unstable \
                         feature is asked for in the source, `@!feature(ffi)` at the top of \
                         the package's root module, and the features a package offers go \
                         under `[features]`"
                            .to_string(),
                    ),
                    _ if from_workspace(key).as_deref() == Some("version") => {
                        inherits.version = true
                    }
                    _ => {}
                }
            }
            "workspace" => {
                let ws = workspace.get_or_insert_default();
                let list = string_array(value);
                match key {
                    "members" => ws.members = list,
                    "exclude" => ws.exclude = list,
                    "default-members" | "default_members" => ws.default_members = list,
                    _ => {}
                }
            }
            "workspace.package" => {
                if key == "version" {
                    workspace.get_or_insert_default().version = Some(unquote(value).to_string());
                }
            }
            // `[features]`: `gpu = ["simd"]` -- a feature, and the others of
            // this package it turns on with it.
            "features" => feature_table.push((key.to_string(), string_array(value))),
            "workspace.dependencies" => {
                if let Some(source) = dep_source(value, key, &mut |w| warnings.push(w)) {
                    let (features, default_features) = dep_features(value);
                    workspace.get_or_insert_default().deps.push(Dependency {
                        name: key.to_string(),
                        source,
                        features,
                        default_features,
                    });
                }
            }
            // `[profile.release]`, `[profile.debug]`, or any other name a
            // driver might come to know. An unknown key is ignored rather than
            // rejected: a manifest written for a later version of the compiler
            // should still build.
            _ if section.starts_with("profile.") => {
                let p = profiles
                    .entry(section["profile.".len()..].to_string())
                    .or_default();
                match key {
                    "opt-level" | "opt_level" => p.opt = OptLevel::parse(unquote(value)),
                    "strictness" => p.strictness = Strictness::parse(unquote(value)),
                    "backend" => p.backend = crate::profile::Backend::parse(unquote(value)),
                    // The command line's `--aot` and `--jit`, as switches.
                    "aot" if unquote(value) == "true" => {
                        p.backend = Some(crate::profile::Backend::Aot)
                    }
                    "jit" if unquote(value) == "true" => {
                        p.backend = Some(crate::profile::Backend::Jit)
                    }
                    "runtime" => p.runtime = crate::aot::Runtime::named(unquote(value)),
                    "threads" => p.threads = unquote(value).parse().ok().filter(|n| *n > 0),
                    "nursery" => p.nursery = crate::profile::nursery_size(unquote(value)).ok(),
                    "leaks" => {
                        p.leaks = match unquote(value) {
                            "true" => Some(true),
                            "false" => Some(false),
                            _ => None,
                        }
                    }
                    "target" => p.target = Some(InternedString::from(unquote(value))),
                    "cycles" => p.cycles = meadow_llvm::Cycles::named(unquote(value)),
                    "cfg" => p.cfg = Some(InternedString::from(unquote(value))),
                    "prune" => {
                        p.prune = match unquote(value) {
                            "true" => Some(true),
                            "false" => Some(false),
                            _ => None,
                        }
                    }
                    "profile" => {
                        p.profile = match unquote(value) {
                            "true" => Some(true),
                            "false" => Some(false),
                            _ => None,
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let manifest = Manifest {
        warnings,
        name,
        version,
        feature_table,
        deps,
        profiles,
        // A manifest with no `[workspace]` is a package's whatever it says,
        // as it always was: the name comes from the directory otherwise.
        is_package: says_package || workspace.is_none(),
        workspace,
        problems,
    };
    (manifest, inherits)
}

/// `text` as whole statements, comments gone: a header, or a `key = value`
/// whose array has been joined onto one line if it ran over several.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut open = 0i32;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let continuing = open > 0;
        // Brackets count only in a value: a `[section]` header balances.
        let counted = if continuing {
            line
        } else {
            line.split_once('=').map_or("", |(_, v)| v)
        };
        let mut in_str = false;
        for c in counted.chars() {
            match c {
                '"' => in_str = !in_str,
                '[' if !in_str => open += 1,
                ']' if !in_str => open -= 1,
                _ => {}
            }
        }
        match out.last_mut() {
            Some(last) if continuing => {
                last.push(' ');
                last.push_str(line);
            }
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// `["a", "b"]` as its strings. Anything else is an empty list.
fn string_array(value: &str) -> Vec<String> {
    let Some(inner) = value
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
    else {
        return Vec::new();
    };
    inner
        .split(',')
        .map(unquote)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `value` is an inline table saying `key = true`.
fn inline_flag(value: &str, key: &str) -> bool {
    value
        .trim()
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .is_some_and(|inner| {
            inner
                .split(',')
                .filter_map(|kv| kv.split_once('='))
                .any(|(k, v)| k.trim() == key && unquote(v) == "true")
        })
}

fn strip_comment(line: &str) -> &str {
    // `#` outside of a quoted string starts a comment.
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

fn unquote(v: &str) -> &str {
    v.trim().trim_matches('"')
}

/// A dependency value is either `"path"` or `{ path = "path" }`.
/// The fields of an inline table, `{ git = "…", tag = "v1" }`.
///
/// Values are quoted, so a `,` inside one would confuse this split. No key a
/// manifest has takes a value with a comma in it -- a URL may, in a query
/// string, but not one that names a repository.
fn inline_fields(value: &str) -> Option<Vec<(&str, &str)>> {
    let v = value.trim();
    let inner = v.strip_prefix('{').and_then(|s| s.strip_suffix('}'))?;
    Some(
        split_fields(inner)
            .into_iter()
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.trim(), unquote(v)))
            .collect(),
    )
}

/// An inline table's fields: cut at its commas, but not at one inside a
/// string or an array -- `features = ["a", "b"]` is one field.
fn split_fields(inner: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quoted, mut from) = (0usize, false, 0usize);
    for (i, c) in inner.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' if !quoted => depth += 1,
            ']' if !quoted => depth = depth.saturating_sub(1),
            ',' if !quoted && depth == 0 => {
                out.push(&inner[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(&inner[from..]);
    out
}

/// What a dependency's inline table says of its features: the ones named,
/// and whether its defaults are taken -- they are unless
/// `default-features = false`.
fn dep_features(value: &str) -> (Vec<String>, bool) {
    let Some(fields) = inline_fields(value) else {
        return (Vec::new(), true);
    };
    let field = |want: &[&str]| {
        fields
            .iter()
            .find(|(k, _)| want.contains(k))
            .map(|(_, v)| *v)
    };
    let features = field(&["features"]).map(string_array).unwrap_or_default();
    let defaults = field(&["default-features", "default_features"]) != Some("false");
    (features, defaults)
}

/// Read one `[dependencies]` value.
///
/// `warn` is told about a spelling that still works but should not be used, so
/// that the manifest can be corrected before it means something else.
fn dep_source(value: &str, key: &str, warn: &mut impl FnMut(String)) -> Option<DepSource> {
    let Some(fields) = inline_fields(value) else {
        // A bare string is a path today. Cargo reads one as a *version*, and
        // Meadow will too once packages can be named rather than located -- so
        // say now, while the two cannot be confused, rather than changing what
        // this manifest means later.
        let path = unquote(value.trim());
        warn(format!(
            "`{key} = \"{path}\"` will mean a version once packages can be \
             named; write `{key} = {{ path = \"{path}\" }}` for a directory"
        ));
        return Some(DepSource::Path(PathBuf::from(path)));
    };
    let field = |want: &str| {
        fields
            .iter()
            .find(|(k, _)| *k == want)
            .map(|(_, v)| (*v).to_string())
    };
    if let Some(url) = field("git") {
        // At most one of these; naming two is a contradiction rather than a
        // precedence question, so it is refused.
        let named: Vec<(&str, String)> = [
            ("branch", "branch"),
            ("tag", "tag"),
            ("rev", "rev"),
            ("version", "version"),
        ]
        .iter()
        .filter_map(|(k, _)| field(k).map(|v| (*k, v)))
        .collect();
        let reference = match named.as_slice() {
            [] => GitRef::Default,
            [("version", v)] => match crate::semver::Req::parse(v) {
                Some(req) => GitRef::Version(req),
                None => {
                    warn(format!(
                        "dependency `{key}` wants version `{v}`, which is not a release                          like `1.2.0`, so it was ignored"
                    ));
                    return None;
                }
            },
            [("branch", b)] => GitRef::Branch(b.clone()),
            [("tag", t)] => GitRef::Tag(t.clone()),
            [("rev", r)] => GitRef::Rev(r.clone()),
            many => {
                let which: Vec<&str> = many.iter().map(|(k, _)| *k).collect();
                warn(format!(
                    "dependency `{key}` names both `{}` -- only one of `branch`, \
                     `tag` and `rev` can be meant, so it was ignored",
                    which.join("` and `")
                ));
                return None;
            }
        };
        // A directory of the repository, under it: never out of it.
        let subdir = match field("subdir") {
            Some(d)
                if Path::new(&d).is_absolute()
                    || Path::new(&d)
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir)) =>
            {
                warn(format!(
                    "dependency `{key}` has `subdir = \"{d}\"`, which is not a directory \
                     inside the repository, so it was ignored"
                ));
                return None;
            }
            other => other,
        };
        return Some(DepSource::Git {
            url,
            reference,
            subdir,
        });
    }
    field("path").map(|p| DepSource::Path(PathBuf::from(p)))
}

fn discover_modules(
    root: &Path,
    pkg_name: InternedString,
) -> Result<Vec<ModuleSource>, Diagnostic> {
    discover_modules_io(root, pkg_name).map_err(|e| match e {
        Discovery::Io(e) => io_diag(root, e),
        Discovery::Misnamed(d) => d,
    })
}

enum Discovery {
    Io(std::io::Error),
    Misnamed(Diagnostic),
}

impl From<std::io::Error> for Discovery {
    fn from(e: std::io::Error) -> Self {
        Discovery::Io(e)
    }
}

fn discover_modules_io(
    root: &Path,
    pkg_name: InternedString,
) -> Result<Vec<ModuleSource>, Discovery> {
    // A single `.mw` file is a one-module package. Its name is exempt from the
    // PascalCase rule below: it is the *package's* name, and a package may be
    // lower-case (`meadow init` makes `app`), whereas a module in a package is a
    // name a `use` has to write.
    if root.is_file() {
        return Ok(vec![module_from_file(root, &[], pkg_name)?]);
    }
    let src_root = {
        let s = root.join("src");
        if s.is_dir() { s } else { root.to_path_buf() }
    };
    let mut files = Vec::new();
    collect_mw(&src_root, &mut files)?;
    files.sort();
    check_module_names(&src_root, &files).map_err(Discovery::Misnamed)?;

    let mut modules = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&src_root).unwrap_or(&file);
        let mut segs: Vec<InternedString> = rel
            .parent()
            .map(|p| {
                p.components()
                    .filter_map(|c| c.as_os_str().to_str())
                    .map(InternedString::from)
                    .collect()
            })
            .unwrap_or_default();
        let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("Mod");
        if !ROOT_STEMS.contains(&stem) {
            segs.push(InternedString::from(stem));
        }
        let name = segs.last().copied().unwrap_or(pkg_name);
        modules.push(module_from_file(&file, &segs, name)?);
    }

    if modules.is_empty() {
        return Err(Discovery::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no .mw modules under {}", src_root.display()),
        )));
    }
    Ok(modules)
}

fn module_from_file(
    file: &Path,
    segs: &[InternedString],
    name: InternedString,
) -> std::io::Result<ModuleSource> {
    let content = std::fs::read_to_string(file)?;
    let source = Source::new(
        SourceKind::File(InternedString::from(file.display().to_string())),
        InternedString::from(content),
    );
    Ok(ModuleSource {
        path: segs.to_vec(),
        name,
        source,
    })
}

fn collect_mw(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_mw(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("mw") {
            out.push(path);
        }
    }
    Ok(())
}

/// A path as the filesystem itself would name it — symlinks and `..` resolved —
/// so that two ways of naming one file compare equal. An editor's URI and a
/// discovered module have to meet somewhere, and this is where.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn package_name(root: &Path) -> InternedString {
    let stem = root
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("package");
    InternedString::from(as_package_name(stem))
}

fn io_diag(root: &Path, e: std::io::Error) -> Diagnostic {
    Diagnostic {
        msg: format!("could not read package at {}: {e}", root.display()),
        filename: root.display().to_string(),
        label: ("here".to_string(), Default::default()),
        extra_labels: vec![],
    }
}

/// File stems that make a module the package's *root* rather than a module of
/// its own name.
const ROOT_STEMS: &[&str] = &["Main", "Lib", "Mod"];

/// Is `s` a module name — PascalCase, as a `use` path segment must be?
///
/// The rule is the lexer's for an upper-case identifier, less the `'` it also
/// allows, which has no business in a file name.
pub fn is_module_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphanumeric())
}

/// The PascalCase spelling of a name, for suggesting a rename: `main` -> `Main`,
/// `p5` -> `P5`, `my-mod` and `my_mod` -> `MyMod`.
fn pascal_case(s: &str) -> String {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(c) => c.to_ascii_uppercase().to_string() + cs.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Every module file, and every directory between it and the source root, has to
/// be PascalCase.
///
/// Both are the same thing to the language: `src/Collections/Vector.mw` is the
/// module `Collections.Vector`, and each segment is a name someone writes in a
/// `use`. A lower-case file used to be accepted and even meant something —
/// `main.mw` was the root — so the message names every offender and what to
/// call it, rather than the first one found.
fn check_module_names(src_root: &Path, files: &[PathBuf]) -> Result<(), Diagnostic> {
    let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
    for file in files {
        let rel = file.strip_prefix(src_root).unwrap_or(file);
        let mut fixed = PathBuf::new();
        let mut bad = false;
        let parts: Vec<_> = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect();
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            let name = if last {
                part.strip_suffix(".mw").unwrap_or(part)
            } else {
                part
            };
            let good = if is_module_name(name) {
                name.to_string()
            } else {
                bad = true;
                pascal_case(name)
            };
            fixed.push(if last { format!("{good}.mw") } else { good });
        }
        if bad {
            renames.push((rel.to_path_buf(), fixed));
        }
    }
    if renames.is_empty() {
        return Ok(());
    }

    // A directory shared by several files would otherwise be reported once per
    // file; the files are what get renamed, so that is fine, but say it once.
    renames.dedup();
    let list = renames
        .iter()
        .map(|(from, to)| format!("`{}` to `{}`", from.display(), to.display()))
        .collect::<Vec<_>>()
        .join(", ");
    let (first, _) = &renames[0];
    Err(Diagnostic {
        msg: format!(
            "module files and directories must be PascalCase, since each is a \
             name in a `use` path — rename {list}"
        ),
        filename: src_root.join(first).display().to_string(),
        label: ("not PascalCase".to_string(), Default::default()),
        extra_labels: vec![],
    })
}

#[cfg(test)]
mod naming_tests {
    use super::*;

    #[test]
    fn a_module_name_is_pascal_case() {
        for good in ["Main", "Lib", "Vector", "P5", "HttpServer"] {
            assert!(is_module_name(good), "{good}");
        }
        for bad in [
            "main", "prelude", "p5", "my-mod", "My_Mod", "", "5p", "Don't",
        ] {
            assert!(!is_module_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_suggested_name_is_what_someone_would_have_meant() {
        assert_eq!(pascal_case("main"), "Main");
        assert_eq!(pascal_case("p5"), "P5");
        assert_eq!(pascal_case("my-mod"), "MyMod");
        assert_eq!(pascal_case("my_mod"), "MyMod");
        assert_eq!(pascal_case("httpServer"), "HttpServer");
    }
}
