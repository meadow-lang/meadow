//! `meadow add` — write a dependency into the manifest.
//!
//! ```text
//! meadow add https://github.com/someone/meadow-json
//! meadow add someone/meadow-json --tag v1.2.0
//! meadow add ../json
//! ```
//!
//! There is no registry to look a name up in, but there does not need to be
//! one: the repository is fetched, and its own `Meadow.toml` says what the
//! package is called. That is the name written into `[dependencies]`, so it is
//! the name the dependency actually has rather than one guessed from a URL.
//!
//! The manifest is edited a line at a time rather than parsed and written back,
//! so comments, ordering and spacing survive. A generated manifest that has
//! lost its comments is a bad trade for a tidier writer.

use crate::git;
use crate::lock::{self, Lock};
use crate::package::{DepSource, GitRef, Manifest};
use crate::status;
use std::path::{Path, PathBuf};

pub struct Options {
    /// A URL, a GitHub `owner/name`, or a directory.
    pub what: String,
    pub reference: GitRef,
    /// Use this name instead of the one the dependency calls itself.
    pub rename: Option<String>,
    /// The package to add to. Its directory, holding `Meadow.toml`.
    pub dir: PathBuf,
}

pub fn run(opts: &Options) -> Result<(), String> {
    let manifest_path = crate::package::manifest_path(&opts.dir)
        .ok_or_else(|| format!("{} has no Meadow.toml", crate::workspace::shown(&opts.dir)))?;

    let source = source_of(&opts.what, &opts.reference, &opts.dir)?;

    // What the dependency calls itself, unless told otherwise. Fetching to find
    // out is the point: the manifest is what knows, not the URL.
    let (name, version, pinned) = match &source {
        DepSource::Path(rel) => {
            let at = opts.dir.join(rel);
            let m = Manifest::load(&at)
                .map_err(|e| format!("could not read {}: {e}", crate::workspace::shown(&at)))?
                .ok_or_else(|| format!("{} has no Meadow.toml", crate::workspace::shown(&at)))?;
            (m.name, m.version, None)
        }
        DepSource::Git { url, reference, .. } => {
            let cache = git::default_cache()?;
            let got = git::ensure(&cache, url, reference, None, crate::package::policy().net)?;
            let m = Manifest::load(&got.path)
                .map_err(|e| format!("could not read the dependency's manifest: {e}"))?
                .ok_or_else(|| {
                    format!("{url} has no Meadow.toml, so it is not a Meadow package")
                })?;
            (m.name, m.version, Some(got))
        }
    };
    let name = opts.rename.clone().unwrap_or(name);

    // Adding what is already there, differently, is a mistake worth stopping
    // for: which one was meant cannot be guessed.
    let existing = Manifest::load(&opts.dir)
        .map_err(|e| format!("could not read the manifest: {e}"))?
        .map(|m| m.deps)
        .unwrap_or_default();
    if let Some(had) = existing.iter().find(|d| d.name == name) {
        if had.source == source {
            status::note("Unchanged", format!("{name} is already a dependency"));
            return Ok(());
        }
        return Err(format!(
            "`{name}` is already a dependency, of something else:\n  \
             it is {}\n  you asked for {}\n\
             Remove it first, or use `--rename` to have both.",
            describe(&had.source),
            describe(&source)
        ));
    }

    let text = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!(
            "could not read {}: {e}",
            crate::workspace::shown(&manifest_path)
        )
    })?;
    let written = insert(&text, &name, &source);
    std::fs::write(&manifest_path, written).map_err(|e| {
        format!(
            "could not write {}: {e}",
            crate::workspace::shown(&manifest_path)
        )
    })?;

    // Pin it now, so the build that follows uses the commit that was just
    // looked at rather than resolving again and perhaps finding another.
    if let (Some(got), Some(id)) = (&pinned, lock::source_id(&source)) {
        let dir = lock::dir_for(&opts.dir);
        let mut lock = Lock::load(&dir);
        lock.insert(lock::Locked {
            name: name.clone(),
            source: id,
            rev: got.rev.clone(),
            tree: got.tree.clone(),
            version: None,
        });
        if let Err(e) = lock.save(&dir) {
            status::warning(format!("could not write {}: {e}", lock::FILE));
        }
    }

    // As cargo says it: what went into the manifest, then what was pinned.
    status::status("Adding", format!("{name} v{version} to dependencies"));
    if let (Some(got), DepSource::Git { url, reference, .. }) = (&pinned, &source) {
        status::status("Locking", "1 package");
        status::status(
            "Adding",
            format!(
                "{name} v{version} ({url}{}#{})",
                reference
                    .written()
                    .map(|r| format!("?{r}"))
                    .unwrap_or_default(),
                git::short(&got.rev)
            ),
        );
    }
    Ok(())
}

/// What `what` names: a directory, or a repository.
fn source_of(what: &str, reference: &GitRef, from: &Path) -> Result<DepSource, String> {
    // A directory that is there is a path dependency, whatever it looks like.
    let as_path = from.join(what);
    if as_path.is_dir() || Path::new(what).is_dir() {
        if *reference != GitRef::Default {
            return Err(format!(
                "`{what}` is a directory, so there is no branch, tag or revision \
                 to take.\n\
                 To use it as a repository rather than as a directory, give it \
                 as a URL: `file://{}`",
                std::fs::canonicalize(&as_path)
                    .unwrap_or_else(|_| as_path.clone())
                    .display()
            ));
        }
        let rel = if as_path.is_dir() {
            PathBuf::from(what)
        } else {
            Path::new(what).to_path_buf()
        };
        return Ok(DepSource::Path(rel));
    }
    let url = url_of(what)?;
    // Nothing was asked for by name, so the dependency takes the release the
    // repository is at -- and, from then on, whatever later release does not
    // break it. A repository with no releases has only its default branch.
    let reference = match reference {
        GitRef::Default => match release_now(&url)? {
            Some(req) => GitRef::Version(req),
            None => GitRef::Default,
        },
        chosen => chosen.clone(),
    };
    Ok(DepSource::Git {
        url,
        reference,
        subdir: None,
    })
}

/// The newest release the repository has tagged, as a requirement to write
/// into the manifest -- or `None` when it has tagged none.
fn release_now(url: &str) -> Result<Option<crate::semver::Req>, String> {
    let cache = git::default_cache()?;
    let have = git::releases(&cache, url, crate::package::policy().net)?;
    let newest = have
        .iter()
        .map(|(v, _)| v)
        .filter(|v| !v.is_prerelease())
        .max()
        .or_else(|| have.iter().map(|(v, _)| v).max());
    match newest {
        Some(v) => Ok(Some(crate::semver::Req { least: v.clone() })),
        None => {
            status::note(
                "Note",
                format!("{url} has tagged no releases, so this follows its default branch"),
            );
            Ok(None)
        }
    }
}

/// The URL `what` means.
///
/// `owner/name` is GitHub, which is where Meadow packages are until there is a
/// registry. It is a convenience of the command line only: the manifest always
/// holds the URL in full, so nothing has to know what the shorthand meant.
fn url_of(what: &str) -> Result<String, String> {
    if what.contains("://") || what.starts_with("git@") {
        return Ok(what.to_string());
    }
    let looks_like_a_repo = |s: &str| {
        let mut parts = s.split('/');
        let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        let ok = |p: &str| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        };
        ok(owner) && ok(name)
    };
    if looks_like_a_repo(what) {
        return Ok(format!("https://github.com/{what}"));
    }
    Err(format!(
        "`{what}` is not a directory, a URL, or a GitHub `owner/name`"
    ))
}

fn describe(source: &DepSource) -> String {
    match source {
        DepSource::Path(p) => format!("the directory {}", p.display()),
        DepSource::Git { url, reference, .. } => match reference.written() {
            Some(r) => format!("{url} ({r})"),
            None => url.clone(),
        },
    }
}

/// How the dependency is written in a manifest.
fn written(source: &DepSource) -> String {
    match source {
        DepSource::Path(p) => format!("{{ path = \"{}\" }}", p.display()),
        DepSource::Git { url, reference, .. } => match reference {
            GitRef::Default => format!("{{ git = \"{url}\" }}"),
            GitRef::Branch(b) => format!("{{ git = \"{url}\", branch = \"{b}\" }}"),
            GitRef::Tag(t) => format!("{{ git = \"{url}\", tag = \"{t}\" }}"),
            GitRef::Rev(r) => format!("{{ git = \"{url}\", rev = \"{r}\" }}"),
            GitRef::Version(req) => format!("{{ git = \"{url}\", version = \"{req}\" }}"),
        },
    }
}

/// `text` with the dependency added.
///
/// Into the existing `[dependencies]` if there is one, at its end; otherwise a
/// new section after everything else. Nothing already written is touched.
fn insert(text: &str, name: &str, source: &DepSource) -> String {
    let line = format!("{name} = {}", written(source));
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let is_section = |l: &str| l.trim_start().starts_with('[');
    let start = lines.iter().position(|l| l.trim() == "[dependencies]");

    let Some(start) = start else {
        // No section yet. A blank line before it, unless the file already ends
        // with one.
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push("[dependencies]".to_string());
        lines.push(line);
        return finish(lines);
    };

    // The end of the section: the line before the next one begins.
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        if is_section(l) {
            end = i;
            break;
        }
    }
    // Back over blank lines, so the entry joins the list rather than following
    // a gap.
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    lines.insert(end, line);
    finish(lines)
}

fn finish(lines: Vec<String>) -> String {
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(url: &str, r: GitRef) -> DepSource {
        DepSource::Git {
            url: url.to_string(),
            reference: r,
            subdir: None,
        }
    }

    #[test]
    fn a_dependency_joins_the_section_that_is_there() {
        let had = "[package]\nname = \"app\"\n\n[dependencies]\nutil = { path = \"../util\" }\n";
        let got = insert(
            had,
            "json",
            &git("https://e.com/j", GitRef::Tag("v1".into())),
        );
        // Directly after the entry already there: one list, no gap in it.
        let want = [
            "[package]",
            "name = \"app\"",
            "",
            "[dependencies]",
            "util = { path = \"../util\" }",
            "json = { git = \"https://e.com/j\", tag = \"v1\" }",
            "",
        ]
        .join("\n");
        assert_eq!(got, want);
    }

    #[test]
    fn a_section_is_made_when_there_is_none() {
        let had = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n";
        let got = insert(had, "json", &git("https://e.com/j", GitRef::Default));
        assert_eq!(
            got,
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\
             json = { git = \"https://e.com/j\" }\n"
        );
    }

    #[test]
    fn what_comes_after_the_section_stays_after_it() {
        let had =
            "[dependencies]\nutil = { path = \"../util\" }\n\n[profile.release]\nopt-level = 2\n";
        let got = insert(had, "json", &git("https://e.com/j", GitRef::Default));
        assert!(
            got.contains("json = { git = \"https://e.com/j\" }\n\n[profile.release]"),
            "{got}"
        );
        assert!(got.trim_end().ends_with("opt-level = 2"), "{got}");
    }

    #[test]
    fn comments_and_spacing_are_left_alone() {
        let had = "# what this package is\n[package]\nname = \"app\"\n\n\
                   [dependencies]\n# the one we already had\nutil = { path = \"../util\" }\n";
        let got = insert(had, "json", &git("https://e.com/j", GitRef::Default));
        assert!(got.starts_with("# what this package is\n"), "{got}");
        assert!(got.contains("# the one we already had\n"), "{got}");
    }

    #[test]
    fn a_github_shorthand_is_a_github_url() {
        assert_eq!(
            url_of("someone/meadow-json").unwrap(),
            "https://github.com/someone/meadow-json"
        );
    }

    #[test]
    fn a_url_is_taken_as_it_is() {
        for url in [
            "https://gitlab.com/someone/thing",
            "git@github.com:someone/thing.git",
            "ssh://git@example.com/thing",
        ] {
            assert_eq!(url_of(url).unwrap(), url);
        }
    }

    #[test]
    fn something_that_is_neither_is_refused_rather_than_guessed_at() {
        assert!(url_of("just-a-word").is_err());
        assert!(url_of("a/b/c").is_err());
    }

    #[test]
    fn each_reference_is_written_the_way_a_manifest_reads_it() {
        assert_eq!(
            written(&git("u", GitRef::Branch("dev".into()))),
            "{ git = \"u\", branch = \"dev\" }"
        );
        assert_eq!(
            written(&git("u", GitRef::Rev("abc".into()))),
            "{ git = \"u\", rev = \"abc\" }"
        );
        assert_eq!(
            written(&DepSource::Path(PathBuf::from("../util"))),
            "{ path = \"../util\" }"
        );
    }
}
