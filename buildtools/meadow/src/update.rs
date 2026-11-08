//! `meadow update` — bring dependencies forward.
//!
//! A manifest says which *reference* a dependency follows -- a branch, a tag,
//! a commit, or a release. The lockfile says which commit that was when it was
//! last looked at. This looks again:
//!
//! ```text
//! meadow update              every dependency
//! meadow update json text    only these
//! meadow update --dry-run    say what would change, change nothing
//! meadow update --breaking   past a release that breaks what is asked for
//! ```
//!
//! What changes is the lockfile, not the manifest: `{ tag = "v1.2.0" }` still
//! means that tag afterwards, and `{ version = "0.2.0" }` takes the newest
//! release that does not break 0.2.0 -- `0.2.5`, not `0.3.0`, as Cargo has it.
//! A dependency pinned with `rev` cannot move at all, which is the point of
//! pinning one.
//!
//! A release that breaks what is asked for is said to be there, since nothing
//! moving toward it would otherwise look like nothing being there to move to.
//! `--breaking` is the one change made to the manifest: each `version` behind
//! such a release is raised to the newest, and the lockfile follows.
//!
//! Each change says which version it went from and to: the release, or, for a
//! dependency following a branch, the version its manifest gives and the
//! commit.
//!
//! Updating the *toolchain* -- which version of Meadow you have -- is
//! `meadowup update`, a different job and a different program. This one is
//! about the package in front of you.

use crate::git;
use crate::lock::{self, Lock};
use crate::package::{DepSource, GitRef, Manifest, PackageGraph, Resolver};
use crate::semver::Version;
use crate::status;
use std::path::{Path, PathBuf};

pub struct Options {
    /// The package, or a workspace member -- the lockfile is found from it.
    pub dir: PathBuf,
    /// Only these dependencies, by name. Empty means all of them.
    pub only: Vec<String>,
    /// Report what would change without changing it.
    pub dry_run: bool,
    /// Raise a `version` past a release that breaks it.
    pub breaking: bool,
    /// Where fetched repositories are kept, when not `$MEADOW_HOME/git`.
    pub cache: Option<PathBuf>,
}

/// One dependency's commit, before and after.
struct Moved {
    name: String,
    source: String,
    from: Option<String>,
    to: String,
    /// The releases, when the dependency asked for a version: what a person
    /// reads instead of two commits.
    was: Option<String>,
    now: Option<String>,
}

pub fn run(opts: &Options) -> Result<(), String> {
    // Run somewhere that is not a package at all, this is most likely someone
    // reaching for what `meadow update` used to mean. Saying where that went
    // helps more than complaining about missing modules.
    let Some(manifest) = crate::package::manifest_path(&opts.dir) else {
        return Err(format!(
            "{} is not a package: it has no Meadow.toml.\n\
             \n\
             `meadow update` brings a package's dependencies forward. To update\n\
             the toolchain itself, that is `meadowup update`.",
            crate::workspace::shown(&opts.dir)
        ));
    };
    let cache = match &opts.cache {
        Some(c) => c.clone(),
        None => git::default_cache()?,
    };
    // Before anything resolves, so that what resolves is the raised version.
    // A dry run raises nothing, and says what it would have.
    let raised = if opts.breaking {
        raise(&opts.dir, &manifest, &opts.only, &cache, opts.dry_run)?
    } else {
        Vec::new()
    };

    let lock_dir = lock::dir_for(&opts.dir);
    let before = Lock::load(&lock_dir);

    // Everything a manifest names is checked, so that a dependency named on the
    // command line is looked for and reported if it is not there.
    let mut resolver = Resolver::for_entry(&lock_dir);
    resolver.cache = cache.clone();
    resolver.update = true;
    resolver.only = opts.only.clone();
    resolver.net = git::Net::Allowed;
    resolver.locked = false;

    let entry: &Path = &opts.dir;
    let graph = PackageGraph::build_all_with(&[entry], &mut resolver)
        .map_err(|d| format!("{}: {}", d.filename, d.msg))?;
    let _ = graph;

    if !opts.only.is_empty() {
        let known: Vec<&str> = resolver.seen.iter().map(|(n, _)| n.as_str()).collect();
        for name in &opts.only {
            if !known.iter().any(|n| n == name) {
                return Err(format!(
                    "`{name}` is not a git dependency of this package.\n\
                     Its dependencies are: {}",
                    if known.is_empty() {
                        "none".to_string()
                    } else {
                        known.join(", ")
                    }
                ));
            }
        }
    }

    let moved = changes(&before, &resolver.lock);
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    status::status(
        "Locking",
        format!(
            "{} package{} to the latest compatible version{}",
            moved.len(),
            plural(moved.len()),
            plural(moved.len())
        ),
    );

    for m in &moved {
        let to = shown(&cache, &m.source, &m.to, m.now.as_deref());
        match &m.from {
            Some(from) => status::status(
                "Updating",
                format!(
                    "{} {} -> {to} ({})",
                    m.name,
                    shown(&cache, &m.source, from, m.was.as_deref()),
                    lock::describes(&m.source)
                ),
            ),
            None => status::status(
                "Adding",
                format!("{} {to} ({})", m.name, lock::describes(&m.source)),
            ),
        }
    }

    // What stayed behind a release that breaks it, which only `--breaking`
    // will move to.
    // A dry run raised nothing, but has said what it would have.
    let mut behind = held_back(&resolver, &opts.only, &cache);
    behind.retain(|(name, _, _)| !raised.contains(name));
    for (name, at, newest) in &behind {
        status::note(
            "Unchanged",
            format!(
                "{name} {at}: {newest} is out, which breaks {at}; \
                 `meadow update --breaking {name}` moves to it"
            ),
        );
    }
    if moved.is_empty() && behind.is_empty() && raised.is_empty() {
        status::note("Unchanged", "every git dependency is already current");
    }
    if opts.dry_run {
        if !raised.is_empty() {
            status::warning("not changing Meadow.toml or the lockfile, as this is a dry run");
        } else if !moved.is_empty() {
            status::warning("not updating the lockfile, as this is a dry run");
        }
        return Ok(());
    }
    if moved.is_empty() {
        return Ok(());
    }
    resolver.lock.retain(&resolver.seen);
    resolver
        .lock
        .save(&lock_dir)
        .map_err(|e| format!("could not write {}: {e}", lock::FILE))?;
    Ok(())
}

/// Raise each `version` the manifest at `path` asks for that a newer release
/// breaks to that release -- of those in `only`, or all of them -- answering
/// the names of those raised.
fn raise(
    dir: &Path,
    path: &Path,
    only: &[String],
    cache: &Path,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let manifest = Manifest::load(dir)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?
        .ok_or_else(|| format!("{} is not a package", dir.display()))?;
    let mut text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut raised = Vec::new();
    for dep in &manifest.deps {
        if !only.is_empty() && !only.contains(&dep.name) {
            continue;
        }
        let DepSource::Git {
            url,
            reference: GitRef::Version(req),
            ..
        } = &dep.source
        else {
            continue;
        };
        let Some(newest) = newest(cache, url, git::Net::Allowed) else {
            continue;
        };
        if newest <= req.least || req.allows(&newest) {
            continue;
        }
        match with_version(&text, &dep.name, &newest.to_string()) {
            Some(t) => {
                text = t;
                raised.push(dep.name.clone());
                status::status(
                    "Upgrading",
                    format!("{} {req} -> {newest} in Meadow.toml", dep.name),
                );
            }
            None => status::warning(format!(
                "{} {req} is behind {newest}, but its `version` is not written in \
                 Meadow.toml where this can change it; change it there by hand",
                dep.name
            )),
        }
    }
    if !raised.is_empty() && !dry_run {
        std::fs::write(path, text)
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    }
    Ok(raised)
}

/// Each dependency that asked for a version and is at the newest release that
/// does not break it, with a newer one out that does: its name, the release it
/// is at, and the newest.
fn held_back(resolver: &Resolver, only: &[String], cache: &Path) -> Vec<(String, String, Version)> {
    let mut out: Vec<(String, String, Version)> = resolver
        .lock
        .packages
        .iter()
        .filter(|p| {
            resolver
                .seen
                .iter()
                .any(|(n, s)| *n == p.name && *s == p.source)
        })
        .filter(|p| only.is_empty() || only.contains(&p.name))
        .filter_map(|p| {
            let at = p.version.as_deref()?;
            let (url, _) = repository(&p.source);
            let newest = newest(cache, &url, git::Net::Offline)?;
            (newest > Version::parse(at)?).then(|| (p.name.clone(), at.to_string(), newest))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    out
}

/// The newest release `url` has tagged, passing over pre-releases.
fn newest(cache: &Path, url: &str, net: git::Net) -> Option<Version> {
    git::releases(cache, url, net)
        .ok()?
        .into_iter()
        .map(|(v, _)| v)
        .filter(|v| !v.is_prerelease())
        .max()
}

/// The repository a lock entry's source is in, and the directory of it the
/// package is, if it is not the top.
fn repository(source: &str) -> (String, Option<String>) {
    let source = source.strip_prefix("git+").unwrap_or(source);
    match source.split_once('?') {
        None => (source.to_string(), None),
        Some((url, query)) => (
            url.to_string(),
            query
                .split('&')
                .find_map(|q| q.strip_prefix("subdir="))
                .map(str::to_string),
        ),
    }
}

/// How a commit of a dependency is shown: the release it was resolved as, or
/// the version its manifest gives there and the commit, or the commit alone.
fn shown(cache: &Path, source: &str, rev: &str, release: Option<&str>) -> String {
    if let Some(v) = release {
        return v.to_string();
    }
    let (url, subdir) = repository(source);
    let path = match subdir {
        Some(d) => format!("{d}/Meadow.toml"),
        None => "Meadow.toml".to_string(),
    };
    match git::file_at(cache, &url, rev, &path).and_then(|t| crate::package::version_in(&t)) {
        Some(v) => format!("{v} ({})", git::short(rev)),
        None => git::short(rev).to_string(),
    }
}

/// `text` with the `version` of the dependency `name` made `to`: in its line of
/// `[dependencies]`, `name = { git = "…", version = "…" }`, or in a section of
/// its own, `[dependencies.name]`. `None` if it is written neither way.
fn with_version(text: &str, name: &str, to: &str) -> Option<String> {
    let mut section = String::new();
    let mut done = false;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let t = line.trim();
            if t.starts_with('[') {
                section = t.to_string();
                return line.to_string();
            }
            if done {
                return line.to_string();
            }
            let key = t.split('=').next().unwrap_or("").trim().trim_matches('"');
            let inline = section == "[dependencies]" && key == name;
            let own = section == format!("[dependencies.{name}]") && key == "version";
            if !(inline || own) {
                return line.to_string();
            }
            match version_replaced(line, to, own) {
                Some(changed) => {
                    done = true;
                    changed
                }
                None => line.to_string(),
            }
        })
        .collect();
    done.then(|| {
        let mut out = lines.join("\n");
        if text.ends_with('\n') {
            out.push('\n');
        }
        out
    })
}

/// `line` with the string after its `version =` made `to`. `whole` when the
/// line is that key and nothing else.
fn version_replaced(line: &str, to: &str, whole: bool) -> Option<String> {
    let at = if whole {
        line.find("version")?
    } else {
        // `version` as a key of the inline table, not inside some string.
        let mut from = 0;
        loop {
            let i = from + line[from..].find("version")?;
            let before = line[..i].trim_end();
            if before.ends_with('{') || before.ends_with(',') {
                break i;
            }
            from = i + "version".len();
        }
    };
    let rest = &line[at + "version".len()..];
    let eq = rest.find('=')?;
    if !rest[..eq].trim().is_empty() {
        return None;
    }
    let open = at + "version".len() + eq + 1 + rest[eq + 1..].find('"')?;
    let close = open + 1 + line[open + 1..].find('"')?;
    Some(format!("{}\"{to}\"{}", &line[..open], &line[close + 1..]))
}

/// What `after` pins that `before` did not, or pinned differently.
fn changes(before: &Lock, after: &Lock) -> Vec<Moved> {
    let mut out: Vec<Moved> = after
        .packages
        .iter()
        .filter_map(|now| {
            // By its source, or -- for a `version` that `--breaking` raised,
            // which is a source of its own -- by its name and repository.
            let had = before.find(&now.name, &now.source).or_else(|| {
                let (url, dir) = repository(&now.source);
                before.packages.iter().find(|h| {
                    h.name == now.name && repository(&h.source) == (url.clone(), dir.clone())
                })
            });
            match had {
                Some(had) if had.rev == now.rev => None,
                had => Some(Moved {
                    name: now.name.clone(),
                    source: now.source.clone(),
                    from: had.map(|h| h.rev.clone()),
                    to: now.rev.clone(),
                    was: had.and_then(|h| h.version.clone()),
                    now: now.version.clone(),
                }),
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::Locked;

    fn locked(name: &str, rev: &str) -> Locked {
        Locked {
            name: name.to_string(),
            source: format!("git+https://e.com/{name}?branch=main"),
            rev: rev.to_string(),
            tree: "t".to_string(),
            version: None,
        }
    }

    #[test]
    fn a_commit_that_did_not_move_is_not_a_change() {
        let mut before = Lock::default();
        before.insert(locked("json", "aaa"));
        let mut after = Lock::default();
        after.insert(locked("json", "aaa"));
        assert!(changes(&before, &after).is_empty());
    }

    #[test]
    fn a_commit_that_moved_is_reported_both_ways() {
        let mut before = Lock::default();
        before.insert(locked("json", "aaa"));
        let mut after = Lock::default();
        after.insert(locked("json", "bbb"));
        let moved = changes(&before, &after);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].from.as_deref(), Some("aaa"));
        assert_eq!(moved[0].to, "bbb");
    }

    #[test]
    fn a_dependency_that_was_not_pinned_before_is_an_addition() {
        let before = Lock::default();
        let mut after = Lock::default();
        after.insert(locked("json", "aaa"));
        let moved = changes(&before, &after);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].from, None);
    }

    #[test]
    fn changes_are_reported_in_a_settled_order() {
        let before = Lock::default();
        let mut after = Lock::default();
        after.insert(locked("text", "bbb"));
        after.insert(locked("json", "aaa"));
        let moved = changes(&before, &after);
        let names: Vec<&str> = moved.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["json", "text"]);
    }

    #[test]
    fn one_name_changing_leaves_the_others_alone() {
        let mut before = Lock::default();
        before.insert(locked("json", "aaa"));
        before.insert(locked("text", "bbb"));
        let mut after = Lock::default();
        after.insert(locked("json", "ccc"));
        after.insert(locked("text", "bbb"));
        let moved = changes(&before, &after);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].name, "json");
    }

    #[test]
    fn a_version_is_raised_where_it_is_written_and_nothing_else_moves() {
        let text = "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\n\
                    Scythe = { git = \"https://github.com/meadow-lang/Scythe\", version = \"0.2.0\" }\n\
                    Other = { git = \"https://e.com/other\", version = \"0.2.0\" }\n";
        let out = with_version(text, "Scythe", "0.3.0").unwrap();
        assert!(
            out.contains(
                "Scythe = { git = \"https://github.com/meadow-lang/Scythe\", version = \"0.3.0\" }"
            ),
            "{out}"
        );
        assert!(out.contains("Other = { git = \"https://e.com/other\", version = \"0.2.0\" }"));
        assert!(
            out.contains("[package]\nname = \"App\"\nversion = \"0.1.0\""),
            "{out}"
        );
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn a_version_in_a_section_of_its_own_is_raised_too() {
        let text = "[dependencies.Scythe]\ngit = \"https://e.com/scythe\"\nversion=\"0.2.0\"\n";
        let out = with_version(text, "Scythe", "0.3.0").unwrap();
        assert!(out.contains("version=\"0.3.0\""), "{out}");
    }

    /// A `version` inside the URL is not the key: `https://e.com/version`.
    #[test]
    fn only_the_key_is_changed_not_a_word_in_a_string() {
        let text = "[dependencies]\nX = { git = \"https://e.com/version\", version = \"1.0.0\" }\n";
        let out = with_version(text, "X", "2.0.0").unwrap();
        assert!(
            out.contains("X = { git = \"https://e.com/version\", version = \"2.0.0\" }"),
            "{out}"
        );
    }

    #[test]
    fn a_dependency_written_with_no_version_is_left_alone() {
        let text = "[dependencies]\nX = { git = \"https://e.com/x\", branch = \"main\" }\n";
        assert_eq!(with_version(text, "X", "2.0.0"), None);
        assert_eq!(with_version(text, "Y", "2.0.0"), None);
    }

    #[test]
    fn a_source_names_its_repository_and_its_directory() {
        assert_eq!(
            repository("git+https://e.com/x?version=0.2.0"),
            ("https://e.com/x".to_string(), None)
        );
        assert_eq!(
            repository("git+https://e.com/x?branch=main&subdir=libs/y"),
            ("https://e.com/x".to_string(), Some("libs/y".to_string()))
        );
        assert_eq!(
            repository("git+https://e.com/x"),
            ("https://e.com/x".to_string(), None)
        );
    }
}
