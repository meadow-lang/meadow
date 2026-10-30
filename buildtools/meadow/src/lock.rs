//! `Meadow.lock` — exactly what a build used.
//!
//! A manifest says what is wanted: `{ git = "…", branch = "main" }`. That is not
//! enough to build the same program twice, because a branch moves and a tag can
//! be moved. The lockfile says what "main" *was*:
//!
//! ```toml
//! version = 1
//!
//! [[package]]
//! name = "json"
//! source = "git+https://github.com/someone/meadow-json?tag=v1.2.0"
//! version = "1.2.0"
//! rev = "a1b2c3d4e5f6…"
//! tree = "4842b356d40d…"
//! ```
//!
//! `rev` is the commit. `tree` is the hash of its source, which is what makes
//! this an integrity check rather than a note: two commits with the same
//! contents share a `tree`, and a commit whose contents changed cannot keep
//! one. Git computes both, so neither costs a dependency.
//!
//! **Commit it.** The lockfile is how a build on another machine, or in CI six
//! months from now, is the build you tested. `meadow update` is how it changes,
//! and that it changes is then a line in a diff rather than a surprise.

use crate::package::DepSource;
use std::path::{Path, PathBuf};

/// The file, as a name a manifest sits beside.
pub const FILE: &str = "Meadow.lock";

/// What it was called before it was spelt as `Meadow.toml` is: read where
/// there is no [`FILE`], and renamed the first time the lock is written.
pub const FORMER: &str = "meadow.lock";

/// Whether `dir` holds a file spelt exactly `name`: what the directory
/// lists, which is how it is spelt whatever the file system makes of case.
fn named_exactly(dir: &Path, name: &str) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_str() == Some(name))
    })
}

/// The format's version, so a later one can be recognised rather than
/// misread. A lockfile from the future is regenerated, not guessed at.
const VERSION: u32 = 1;

/// One dependency, as it was resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locked {
    pub name: String,
    /// What was asked for: `git+<url>?tag=v1`. Two dependencies with the same
    /// name and different sources are different entries.
    pub source: String,
    /// The commit.
    pub rev: String,
    /// The hash of the source tree at that commit.
    pub tree: String,
    /// The release this resolved to, when the manifest asked for a version
    /// rather than a reference. What moved is then a version in a diff, not a
    /// commit nobody can read.
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lock {
    pub packages: Vec<Locked>,
}

impl Lock {
    /// Read the lockfile beside `dir`, or an empty one when there is none --
    /// which is what a first build sees.
    pub fn load(dir: &Path) -> Lock {
        let text = std::fs::read_to_string(dir.join(FILE))
            .or_else(|_| std::fs::read_to_string(dir.join(FORMER)));
        match text {
            Ok(text) => Lock::parse(&text),
            Err(_) => Lock::default(),
        }
    }

    pub fn parse(text: &str) -> Lock {
        let mut packages: Vec<Locked> = Vec::new();
        let mut version_ok = true;
        let mut current: Option<Locked> = None;
        let finish = |current: &mut Option<Locked>, into: &mut Vec<Locked>| {
            if let Some(p) = current.take()
                && !p.name.is_empty()
                && !p.rev.is_empty()
            {
                into.push(p);
            }
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[[package]]" {
                finish(&mut current, &mut packages);
                current = Some(Locked {
                    name: String::new(),
                    source: String::new(),
                    rev: String::new(),
                    tree: String::new(),
                    version: None,
                });
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), unquote(value.trim()));
            match (&mut current, key) {
                (None, "version") => version_ok = value == VERSION.to_string(),
                (Some(p), "name") => p.name = value,
                (Some(p), "source") => p.source = value,
                (Some(p), "rev") => p.rev = value,
                (Some(p), "tree") => p.tree = value,
                (Some(p), "version") => p.version = Some(value),
                _ => {}
            }
        }
        finish(&mut current, &mut packages);
        // A version this does not know is not read at all: guessing at it could
        // pin the wrong commit, and resolving afresh is always safe.
        if !version_ok {
            return Lock::default();
        }
        Lock { packages }
    }

    /// What was recorded for `name` from `source`.
    pub fn find(&self, name: &str, source: &str) -> Option<&Locked> {
        self.packages
            .iter()
            .find(|p| p.name == name && p.source == source)
    }

    /// Record a resolved dependency, replacing any earlier entry for it.
    pub fn insert(&mut self, entry: Locked) {
        match self
            .packages
            .iter_mut()
            .find(|p| p.name == entry.name && p.source == entry.source)
        {
            Some(slot) => *slot = entry,
            None => self.packages.push(entry),
        }
    }

    /// Drop everything not in `keep`: a dependency no longer written in any
    /// manifest should not be pinned for ever.
    pub fn retain(&mut self, keep: &[(String, String)]) {
        self.packages
            .retain(|p| keep.iter().any(|(n, s)| *n == p.name && *s == p.source));
    }

    pub fn render(&self) -> String {
        let mut sorted = self.packages.clone();
        // Sorted, so that two machines resolving the same graph write the same
        // file and a diff shows a change rather than a reordering.
        sorted.sort_by(|a, b| (&a.name, &a.source).cmp(&(&b.name, &b.source)));
        let mut out = String::from(
            "# Written by meadow. Commit this file: it is what makes a build on\n\
             # another machine the build you tested. Change it with `meadow update`.\n\n",
        );
        out.push_str(&format!("version = {VERSION}\n"));
        for p in &sorted {
            out.push_str("\n[[package]]\n");
            out.push_str(&format!("name = \"{}\"\n", p.name));
            out.push_str(&format!("source = \"{}\"\n", p.source));
            if let Some(v) = &p.version {
                out.push_str(&format!("version = \"{v}\"\n"));
            }
            out.push_str(&format!("rev = \"{}\"\n", p.rev));
            if !p.tree.is_empty() {
                out.push_str(&format!("tree = \"{}\"\n", p.tree));
            }
        }
        out
    }

    /// Write the lockfile beside `dir`, but only when it would change: an
    /// untouched file keeps its timestamp, and nothing rebuilds for nothing.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let path: PathBuf = dir.join(FILE);
        // One written under the former name is this file from now on. A
        // rename, not a second file: where names differ only by case and the
        // file system does not tell them apart, writing the new name would
        // leave the old spelling on disk.
        if named_exactly(dir, FORMER) {
            std::fs::rename(dir.join(FORMER), &path)?;
        }
        let text = self.render();
        if std::fs::read_to_string(&path).is_ok_and(|had| had == text) {
            return Ok(());
        }
        std::fs::write(path, text)
    }
}

/// Where the lockfile for a build rooted at `entry` belongs.
///
/// A workspace has one lockfile at its root, not one per member: its members
/// are resolved together, and pinning them apart would let two of them build
/// against different commits of one dependency.
pub fn dir_for(entry: &Path) -> PathBuf {
    let start = if entry.is_dir() {
        entry.to_path_buf()
    } else {
        entry.parent().unwrap_or(entry).to_path_buf()
    };
    for dir in start.ancestors() {
        if crate::workspace::declares_workspace(dir) {
            return dir.to_path_buf();
        }
    }
    start
        .ancestors()
        .find(|d| crate::workspace::has_manifest(d))
        .map(Path::to_path_buf)
        .unwrap_or(start)
}

/// How a dependency's source is written in the lockfile.
///
/// A path dependency has none: it is whatever is in that directory, which the
/// lockfile has no way to pin and no business trying to.
pub fn source_id(source: &DepSource) -> Option<String> {
    match source {
        DepSource::Path(_) => None,
        DepSource::Git {
            url,
            reference,
            subdir,
        } => {
            // A directory of the repository is a package of its own: two from
            // one repository are two sources.
            let mut query: Vec<String> = reference.written().into_iter().collect();
            if let Some(d) = subdir {
                query.push(format!("subdir={d}"));
            }
            Some(if query.is_empty() {
                format!("git+{url}")
            } else {
                format!("git+{url}?{}", query.join("&"))
            })
        }
    }
}

/// The reference part of a source id, for a message.
pub fn describes(source: &str) -> String {
    match source.split_once('?') {
        Some((url, r)) => format!("{} ({r})", url.trim_start_matches("git+")),
        None => source.trim_start_matches("git+").to_string(),
    }
}

fn unquote(s: &str) -> String {
    s.trim()
        .trim_start_matches('"')
        .trim_end_matches('"')
        .to_string()
}

/// A dependency that the lockfile pinned but the manifest no longer matches.
pub fn changed(what: &str) -> String {
    format!(
        "{what} is not what Meadow.lock pins, and `--locked` was given.\n\
         Run `meadow update` to change the lockfile deliberately."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::GitRef;

    fn entry(name: &str, rev: &str) -> Locked {
        Locked {
            name: name.to_string(),
            source: format!("git+https://example.com/{name}?tag=v1"),
            rev: rev.to_string(),
            tree: "tree0".to_string(),
            version: None,
        }
    }

    #[test]
    fn what_is_written_is_what_is_read_back() {
        let mut lock = Lock::default();
        lock.insert(entry("json", "aaa"));
        lock.insert(entry("text", "bbb"));
        assert_eq!(Lock::parse(&lock.render()), lock_of(&lock));
    }

    /// `render` sorts, so compare against the sorted form.
    fn lock_of(lock: &Lock) -> Lock {
        let mut sorted = lock.packages.clone();
        sorted.sort_by(|a, b| (&a.name, &a.source).cmp(&(&b.name, &b.source)));
        Lock { packages: sorted }
    }

    #[test]
    fn writing_is_stable_whatever_order_things_were_resolved_in() {
        let mut one = Lock::default();
        one.insert(entry("text", "bbb"));
        one.insert(entry("json", "aaa"));
        let mut other = Lock::default();
        other.insert(entry("json", "aaa"));
        other.insert(entry("text", "bbb"));
        assert_eq!(one.render(), other.render());
    }

    #[test]
    fn resolving_again_replaces_rather_than_repeats() {
        let mut lock = Lock::default();
        lock.insert(entry("json", "aaa"));
        lock.insert(entry("json", "ccc"));
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].rev, "ccc");
    }

    #[test]
    fn two_sources_of_one_name_are_two_entries() {
        let mut lock = Lock::default();
        lock.insert(entry("json", "aaa"));
        lock.insert(Locked {
            source: "git+https://elsewhere.example/json?tag=v2".to_string(),
            ..entry("json", "bbb")
        });
        assert_eq!(lock.packages.len(), 2);
    }

    #[test]
    fn a_lockfile_from_a_later_meadow_is_not_guessed_at() {
        // Reading it wrongly could pin the wrong commit. Resolving afresh
        // cannot be wrong, only slower.
        let text = "version = 99\n\n[[package]]\nname = \"json\"\nsource = \"s\"\nrev = \"r\"\n";
        assert!(Lock::parse(text).packages.is_empty());
    }

    #[test]
    fn a_dependency_nobody_wants_any_more_is_dropped() {
        let mut lock = Lock::default();
        lock.insert(entry("json", "aaa"));
        lock.insert(entry("text", "bbb"));
        let keep = vec![("json".to_string(), entry("json", "").source)];
        lock.retain(&keep);
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "json");
    }

    /// A lock written under the name it had before is read, and is the
    /// lock under its name now once it is written: one file, spelt as the
    /// manifest is.
    #[test]
    fn a_lock_under_its_former_name_is_read_and_renamed() {
        let dir = meadow_scratch::scratch!("meadow-lock-{}", std::process::id());
        let mut lock = Lock::default();
        lock.packages.push(Locked {
            name: "Dep".to_string(),
            source: "git+https://example.com/dep?branch=master".to_string(),
            version: None,
            rev: "a".repeat(40),
            tree: "b".repeat(40),
        });
        std::fs::write(dir.join(FORMER), lock.render()).unwrap();
        let read = Lock::load(&dir);
        assert_eq!(read.packages, lock.packages, "read under its former name");
        let mut changed = read;
        changed.packages[0].rev = "c".repeat(40);
        changed.save(&dir).unwrap();
        assert!(named_exactly(&dir, FILE), "written under its name");
        assert!(
            !named_exactly(&dir, FORMER),
            "and not left under the old one"
        );
        assert_eq!(Lock::load(&dir).packages, changed.packages);
    }

    #[test]
    fn a_path_dependency_has_nothing_to_pin() {
        assert_eq!(
            source_id(&DepSource::Path(std::path::PathBuf::from("../util"))),
            None
        );
    }

    #[test]
    fn a_source_id_says_which_reference_was_asked_for() {
        let with = source_id(&DepSource::Git {
            url: "https://example.com/j".to_string(),
            reference: GitRef::Tag("v1".to_string()),
            subdir: None,
        });
        assert_eq!(with.as_deref(), Some("git+https://example.com/j?tag=v1"));
        let plain = source_id(&DepSource::Git {
            url: "https://example.com/j".to_string(),
            reference: GitRef::Default,
            subdir: None,
        });
        assert_eq!(plain.as_deref(), Some("git+https://example.com/j"));
        let under = source_id(&DepSource::Git {
            url: "https://example.com/j".to_string(),
            reference: GitRef::Tag("v1".to_string()),
            subdir: Some("lib/j".to_string()),
        });
        assert_eq!(
            under.as_deref(),
            Some("git+https://example.com/j?tag=v1&subdir=lib/j")
        );
    }
}
