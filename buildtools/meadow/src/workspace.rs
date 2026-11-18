//! **Workspaces**: several packages developed together, as in Cargo.
//!
//! A workspace is a directory whose `Meadow.toml` has a `[workspace]` section
//! naming its members:
//!
//! ```toml
//! [workspace]
//! members = ["app", "libs/*"]     # directories; `*` and `?` match in a segment
//! exclude = ["libs/old"]          # never members, whatever matches them
//! default-members = ["app"]       # what a command at the root builds
//!
//! [workspace.package]
//! version = "0.2.0"               # for a member's `version.workspace = true`
//!
//! [workspace.dependencies]
//! util = { path = "libs/util" }   # for a member's `util = { workspace = true }`
//!
//! [profile.release]               # every member builds with these
//! opt-level = 2
//! ```
//!
//! The root may be a package too, with a `[package]` of its own; without one
//! the manifest is *virtual* and only says what the workspace is.
//!
//! What being a member means:
//!
//! - **One `target`.** Every member builds into the workspace root's `target`
//!   directory, so a library several members use is written once.
//! - **One set of profiles.** `[profile.*]` is read from the root manifest;
//!   a member's own is ignored, with a warning.
//! - **Built together.** `meadow build --workspace` and `meadow test
//!   --workspace` compile every package the members reach once, and `-p NAME`
//!   picks members by name from anywhere inside the workspace.
//!
//! A path dependency of a member that lives under the root is a member too,
//! unless `exclude` says otherwise -- which is Cargo's rule, and saves listing
//! a library in two places. A package under the root that is neither is an
//! error, rather than a package silently built with the wrong profiles into
//! the wrong `target`.

use crate::package::{Manifest, canonical};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Workspace {
    /// The directory holding the `[workspace]` manifest, canonical.
    pub root: PathBuf,
    /// That manifest.
    pub manifest: Manifest,
    /// Every member, the root package first if there is one, then in the
    /// order `members` lists them, then the path dependencies that joined.
    pub members: Vec<Member>,
    /// Indices into `members`: what a command at the root means.
    pub default_members: Vec<usize>,
    /// The directories `exclude` names, canonical.
    excluded: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    /// Canonical.
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Workspace {
    /// The workspace whose root manifest is in `root`.
    pub fn load(root: &Path) -> Result<Workspace, String> {
        let root = canonical(root);
        let manifest = Manifest::load(&root)
            .map_err(|e| format!("could not read {}: {e}", shown(&root.join("Meadow.toml"))))?
            .ok_or_else(|| format!("{} has no Meadow.toml", shown(&root)))?;
        let Some(ws) = manifest.workspace.clone() else {
            return Err(format!(
                "{} has no `[workspace]` section",
                shown(&root.join("Meadow.toml"))
            ));
        };
        let excluded: Vec<PathBuf> = ws.exclude.iter().flat_map(|p| expand(&root, p)).collect();

        let mut dirs: Vec<PathBuf> = Vec::new();
        if manifest.is_package {
            dirs.push(root.clone());
        }
        for pattern in &ws.members {
            let found = expand(&root, pattern);
            if !is_pattern(pattern) && found.is_empty() {
                return Err(format!(
                    "workspace member `{pattern}` of {} does not exist",
                    shown(&root)
                ));
            }
            for dir in found {
                if has_manifest(&dir) {
                    push_new(&mut dirs, dir);
                } else if !is_pattern(pattern) {
                    return Err(format!(
                        "workspace member `{pattern}` has no Meadow.toml at {}",
                        shown(&dir)
                    ));
                }
            }
        }
        dirs.retain(|d| *d == root || !excluded.contains(d));

        // Path dependencies under the root join, and theirs, and so on.
        let mut members: Vec<Member> = Vec::new();
        let mut i = 0;
        while i < dirs.len() {
            let dir = dirs[i].clone();
            let m = if dir == root {
                manifest.clone()
            } else {
                Manifest::load(&dir)
                    .map_err(|e| format!("could not read {}: {e}", shown(&dir)))?
                    .ok_or_else(|| format!("{} has no Meadow.toml", shown(&dir)))?
            };
            for d in &m.deps {
                // Only a path dependency can be a member of this workspace; a
                // git one lives somewhere else by definition.
                let crate::package::DepSource::Path(rel) = &d.source else {
                    continue;
                };
                let dep = canonical(&dir.join(rel));
                if dep.starts_with(&root) && !excluded.contains(&dep) && has_manifest(&dep) {
                    push_new(&mut dirs, dep);
                }
            }
            members.push(Member {
                name: m.name.clone(),
                dir,
                manifest: m,
            });
            i += 1;
        }

        for (i, a) in members.iter().enumerate() {
            if let Some(b) = members[i + 1..].iter().find(|b| b.name == a.name) {
                return Err(format!(
                    "two members of the workspace at {} are called `{}`: {} and {}",
                    shown(&root),
                    a.name,
                    shown(&a.dir),
                    shown(&b.dir)
                ));
            }
        }

        let mut default_members = Vec::new();
        for pattern in &ws.default_members {
            for dir in expand(&root, pattern) {
                match members.iter().position(|m| m.dir == dir) {
                    Some(i) => {
                        if !default_members.contains(&i) {
                            default_members.push(i)
                        }
                    }
                    None => {
                        return Err(format!(
                            "`default-members` names {}, which is not a member of the workspace",
                            shown(&dir)
                        ));
                    }
                }
            }
        }

        Ok(Workspace {
            root,
            manifest,
            members,
            default_members,
            excluded,
        })
    }

    /// The workspace `path` is part of: the nearest directory at or above it
    /// with a `[workspace]` manifest. `None` when there is none, and an error
    /// when the package `path` is in lies under a workspace without being a
    /// member of it.
    ///
    /// `path` is anything a command is given: a package directory, a file in
    /// one, or the workspace root itself.
    pub fn find(path: &Path) -> Result<Option<Workspace>, String> {
        let start = canonical(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
        let start_dir = if start.is_file() {
            match start.parent() {
                Some(p) => p.to_path_buf(),
                None => return Ok(None),
            }
        } else {
            start.clone()
        };
        // The package `path` is in, if it has a manifest.
        let package = start_dir
            .ancestors()
            .find(|d| has_manifest(d))
            .map(canonical);
        for dir in start_dir.ancestors() {
            if !declares_workspace(dir) {
                continue;
            }
            let ws = Workspace::load(dir)?;
            if let Some(package) = &package
                && *package != ws.root
                && ws.member_at(package).is_none()
            {
                if ws.excluded.iter().any(|e| package.starts_with(e)) {
                    return Ok(None);
                }
                return Err(format!(
                    "the package at {} is inside the workspace at {} but is not a \
                     member of it: add it to `members`, or to `exclude` to keep it out",
                    shown(&package),
                    shown(&ws.root)
                ));
            }
            return Ok(Some(ws));
        }
        Ok(None)
    }

    pub fn member_at(&self, dir: &Path) -> Option<&Member> {
        self.members.iter().find(|m| m.dir == dir)
    }

    pub fn member_named(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }

    /// Every member's name, for a message.
    pub fn names(&self) -> String {
        self.members
            .iter()
            .map(|m| format!("`{}`", m.name))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The members among `paths` -- anything a [`Selected`] holds -- whose
    /// own `[profile.*]` sections go unread, for a warning.
    pub fn ignored_profiles(&self, paths: &[PathBuf]) -> Vec<&Member> {
        let mut found: Vec<&Member> = Vec::new();
        for path in paths {
            let dir = canonical(&std::path::absolute(path).unwrap_or_else(|_| path.clone()));
            let member = dir.ancestors().find_map(|d| self.member_at(d));
            if let Some(m) = member
                && m.dir != self.root
                && !m.manifest.profiles.is_empty()
                && !found.iter().any(|f| f.dir == m.dir)
            {
                found.push(m);
            }
        }
        found
    }
}

/// What a command should build: `-p`, `--workspace` and `--exclude`.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// Members by name.
    pub packages: Vec<String>,
    /// Every member.
    pub workspace: bool,
    /// With `workspace`: members to leave out, by name.
    pub exclude: Vec<String>,
}

/// What a [`Selection`] picked.
#[derive(Debug, Clone)]
pub struct Selected {
    pub workspace: Option<Workspace>,
    /// Package directories -- or the file or directory given, unchanged,
    /// when it is not a workspace member being chosen by name.
    pub paths: Vec<PathBuf>,
}

impl Selection {
    /// The packages this selection means for a command given `path`.
    ///
    /// Without `-p` or `--workspace`, `path` means itself -- unless it is a
    /// workspace's root, where it means `default-members`, or else the root
    /// package, or else (in a virtual workspace) every member. A directory
    /// with no manifest of its own means the package or workspace above it
    /// (see [`crate::package::package_root`]), and is an error outside one.
    pub fn select(&self, path: &Path) -> Result<Selected, String> {
        let path = &crate::package::package_root(path)?;
        let workspace = Workspace::find(path)?;
        let Some(ws) = workspace else {
            if self.workspace {
                return Err(format!("{} is not in a workspace", shown(&path)));
            }
            if !self.packages.is_empty() {
                // `-p` naming the package itself is harmless.
                let own = Manifest::find(path).map(|m| m.name);
                if let Some(bad) = self.packages.iter().find(|p| Some(*p) != own.as_ref()) {
                    return Err(format!(
                        "no package `{bad}` here: {} is not in a workspace",
                        shown(&path)
                    ));
                }
            }
            return Ok(Selected {
                workspace: None,
                paths: vec![path.to_path_buf()],
            });
        };

        let paths: Vec<PathBuf> = if self.workspace {
            for name in &self.exclude {
                if ws.member_named(name).is_none() {
                    return Err(unknown(&ws, name));
                }
            }
            ws.members
                .iter()
                .filter(|m| !self.exclude.contains(&m.name))
                .map(|m| m.dir.clone())
                .collect()
        } else if !self.packages.is_empty() {
            let mut paths = Vec::new();
            for name in &self.packages {
                let m = ws.member_named(name).ok_or_else(|| unknown(&ws, name))?;
                push_new(&mut paths, m.dir.clone());
            }
            paths
        } else {
            let here = canonical(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
            if here != ws.root {
                vec![path.to_path_buf()]
            } else if !ws.default_members.is_empty() {
                ws.default_members
                    .iter()
                    .map(|&i| ws.members[i].dir.clone())
                    .collect()
            } else if ws.manifest.is_package {
                vec![ws.root.clone()]
            } else {
                ws.members.iter().map(|m| m.dir.clone()).collect()
            }
        };
        if paths.is_empty() {
            return Err(format!(
                "the workspace at {} has no members to build",
                shown(&ws.root)
            ));
        }
        Ok(Selected {
            workspace: Some(ws),
            paths,
        })
    }
}

impl Selected {
    /// The one package a command that runs one needs -- `meadow run`.
    pub fn one(&self, command: &str) -> Result<&Path, String> {
        match (&self.paths[..], &self.workspace) {
            ([one], _) => Ok(one),
            (_, Some(ws)) => Err(format!(
                "`meadow {command}` needs one package, and this means {}: choose one \
                 of {} with `-p NAME`, or set `default-members`",
                self.paths.len(),
                ws.names()
            )),
            _ => unreachable!("only a workspace selects several packages"),
        }
    }
}

fn unknown(ws: &Workspace, name: &str) -> String {
    format!(
        "the workspace at {} has no member `{name}`; its members are {}",
        shown(&ws.root),
        ws.names()
    )
}

/// A path for a message: without the `\\?\` canonicalising puts on Windows.
pub fn shown(path: &Path) -> String {
    crate::dap::session::plain_path(&path.display().to_string())
}

fn push_new(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if !dirs.contains(&dir) {
        dirs.push(dir);
    }
}

pub(crate) fn has_manifest(dir: &Path) -> bool {
    dir.join("Meadow.toml").is_file() || dir.join("Meadow.pkg").is_file()
}

/// Whether `dir` holds a manifest with a `[workspace]` section -- without the
/// whole of [`Workspace::load`], since every ancestor of every path asks.
pub(crate) fn declares_workspace(dir: &Path) -> bool {
    Manifest::load(dir)
        .ok()
        .flatten()
        .is_some_and(|m| m.workspace.is_some())
}

fn is_pattern(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

/// The directories `pattern` names under `root`, canonical and sorted, where
/// a segment may use `*` (any run of characters) and `?` (any one).
fn expand(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for segment in pattern
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
    {
        let mut next = Vec::new();
        for dir in &dirs {
            if !is_pattern(segment) {
                next.push(dir.join(segment));
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut found: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter(|e| e.file_name().to_str().is_some_and(|n| wildcard(segment, n)))
                .map(|e| e.path())
                .collect();
            found.sort();
            next.extend(found);
        }
        dirs = next;
    }
    dirs.into_iter()
        .filter(|d| d.is_dir())
        .map(|d| canonical(&d))
        .collect()
}

/// Whether `name` matches `pattern`'s `*` and `?`.
fn wildcard(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some(('?', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some((c, rest)) => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

/// Add `member` -- a directory relative to the root, with `/` between
/// segments -- to the `members` of the workspace manifest `text`, answering
/// the new text. Whether it is a member already is the caller's question.
///
/// The edit is textual, so that comments and layout survive: into an existing
/// `members = [...]`, on one line or several, or as a new `members` line
/// straight after `[workspace]`.
pub fn add_member(text: &str, member: &str) -> String {
    let quoted = format!("\"{member}\"");
    let lines: Vec<&str> = text.lines().collect();
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

    let header = lines.iter().position(|l| l.trim() == "[workspace]");
    let Some(header) = header else {
        let sep = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            newline
        };
        return format!("{text}{sep}{newline}[workspace]{newline}members = [{quoted}]{newline}");
    };
    let section_end = lines[header + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |i| header + 1 + i);
    let members = (header + 1..section_end).find(|&i| {
        lines[i]
            .split_once('=')
            .is_some_and(|(k, _)| k.trim() == "members")
    });
    match members {
        None => out.insert(header + 1, format!("members = [{quoted}]")),
        Some(start) => {
            // The line the array closes on.
            let close = (start..section_end)
                .find(|&i| strip(lines[i]).contains(']'))
                .unwrap_or(start);
            let line = strip(lines[close]);
            let at = line.rfind(']').expect("found above");
            let before = line[..at].trim_end();
            if close == start {
                let empty = before.ends_with('[');
                let sep = if empty || before.ends_with(',') {
                    ""
                } else {
                    ", "
                };
                out[close] = format!("{before}{sep}{quoted}{}", &lines[close][at..]);
            } else if before.is_empty() {
                // `]` on a line of its own: a new line before it, lined up with
                // the one above, which gains a comma if it lacks one.
                let above = close - 1;
                let prev = strip(lines[above]).trim_end();
                if !prev.ends_with(',') && !prev.ends_with('[') {
                    out[above] = format!("{prev},");
                }
                let indent: String = if above == start {
                    "    ".to_string()
                } else {
                    lines[above]
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .collect()
                };
                out.insert(close, format!("{indent}{quoted},"));
            } else {
                let sep = if before.ends_with(',') { " " } else { ", " };
                out[close] = format!("{before}{sep}{quoted}{}", &lines[close][at..]);
            }
        }
    }
    let mut joined = out.join(newline);
    if text.ends_with('\n') {
        joined.push_str(newline);
    }
    joined
}

/// A line without its comment.
fn strip(line: &str) -> &str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_matches_within_a_segment() {
        assert!(wildcard("*", "app"));
        assert!(wildcard("lib-*", "lib-util"));
        assert!(!wildcard("lib-*", "app"));
        assert!(wildcard("a?c", "abc"));
        assert!(!wildcard("a?c", "ac"));
    }

    #[test]
    fn a_member_is_added_where_the_list_is() {
        assert_eq!(
            add_member("[workspace]\nmembers = []\n", "app"),
            "[workspace]\nmembers = [\"app\"]\n"
        );
        assert_eq!(
            add_member("[workspace]\nmembers = [\"a\"] # mine\n", "b"),
            "[workspace]\nmembers = [\"a\", \"b\"] # mine\n"
        );
        assert_eq!(
            add_member("[workspace]\nmembers = [\n    \"a\"\n]\n", "b"),
            "[workspace]\nmembers = [\n    \"a\",\n    \"b\",\n]\n"
        );
        assert_eq!(
            add_member("[workspace]\nmembers = [\n  \"a\",\n]\n", "b"),
            "[workspace]\nmembers = [\n  \"a\",\n  \"b\",\n]\n"
        );
        assert_eq!(
            add_member("[workspace]\nexclude = []\n\n[profile.debug]\n", "b"),
            "[workspace]\nmembers = [\"b\"]\nexclude = []\n\n[profile.debug]\n"
        );
    }
}
