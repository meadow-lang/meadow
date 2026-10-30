//! Fetching a dependency out of a git repository.
//!
//! There is no central registry yet, so a dependency is named by the repository
//! it lives in:
//!
//! ```toml
//! [dependencies]
//! json = { git = "https://github.com/someone/meadow-json", tag = "v1.2.0" }
//! ```
//!
//! This shells out to `git` rather than linking a git library. That is one
//! fewer dependency, and it inherits the user's ssh keys and credential helper,
//! so a private repository works without Meadow knowing anything about
//! authentication.
//!
//! # What is kept where
//!
//! Under `$MEADOW_HOME/git`, so that one fetch serves every project:
//!
//! ```text
//! db/<slug>/           a bare clone, fetched into
//! checkouts/<slug>/<rev>/   one directory per commit, never written again
//! ```
//!
//! A checkout is named by the commit, so it never has to be updated -- a
//! different commit is a different directory. That is what lets a build with a
//! lockfile touch the network not at all.
//!
//! # Why the commit is written down
//!
//! `branch = "main"` moves, and a tag *can* be moved -- `git push --force`
//! rewrites what `v1.2.0` means. So what a build resolved is recorded in
//! `Meadow.lock` and checked on the way back: if a tag now names a different
//! commit, that is an error rather than a silently different program. This is
//! the integrity check for a git dependency, and git computes it for us, since
//! a commit name *is* a hash of everything reachable from it.

use crate::package::GitRef;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether `url` is one a dependency may name, refusing what git would take
/// as an option or a remote-helper command rather than a location to fetch.
///
/// A URL beginning with `-` is an option to `git clone`/`git fetch` -- and
/// `--upload-pack=<cmd>` runs `<cmd>` -- so a manifest could otherwise run a
/// command at build time. A `helper::address` URL (`ext::sh -c …`) runs a
/// command through git's remote-helper machinery. Both are refused here; only
/// the ordinary transports, the scp-like `user@host:path` form, and a path to
/// a repository on this machine are allowed.
pub fn safe_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("a git dependency has an empty url".to_string());
    }
    if url.starts_with('-') {
        return Err(format!(
            "the git url `{url}` begins with `-`, so git would read it as an \
             option rather than a location; refusing it"
        ));
    }
    // `helper::address` -- a remote-helper transport, which can run a command.
    if let Some(i) = url.find("::") {
        let scheme = &url[..i];
        if !scheme.is_empty()
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'.' || b == b'-')
        {
            return Err(format!(
                "the git url `{url}` uses the `{scheme}::` transport, which can \
                 run a command; refusing it"
            ));
        }
    }
    const SCHEMES: [&str; 5] = ["https://", "http://", "ssh://", "git://", "file://"];
    if SCHEMES.iter().any(|s| url.starts_with(s)) {
        return Ok(());
    }
    // A repository on this machine, by its path: `/srv/repo`, `./repo`,
    // `../repo`, `C:\repo`. Git reads it as a path, and neither of the two
    // dangers above can be one.
    let b = url.as_bytes();
    let drive =
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\');
    if url.starts_with('/') || url.starts_with("./") || url.starts_with("../") || drive {
        return Ok(());
    }
    // scp-like: `[user@]host:path`, the host part holding no slash.
    if let Some(colon) = url.find(':')
        && !url[..colon].contains('/')
        && !url[..colon].is_empty()
    {
        return Ok(());
    }
    Err(format!(
        "`{url}` is not a git url meadow understands; expected one beginning \
         with https://, ssh://, git:// or file://, a `host:path` address, or a \
         path to a repository"
    ))
}

/// Whether `s` is a full commit id -- 40 hex characters, or 64 for sha256.
/// A lockfile's `rev` becomes a directory name and a revision handed to git,
/// so one that is a path (absolute, or holding `..` or a separator) must not
/// be trusted.
fn is_commit_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether a build may reach the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Net {
    /// Fetch when what is asked for is not already here.
    #[default]
    Allowed,
    /// Never fetch. What is already cached is used; anything else is an error
    /// saying what would have had to be fetched.
    Offline,
}

/// A git dependency, resolved to something on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The directory holding the dependency's source.
    pub path: PathBuf,
    /// The commit it is, in full.
    pub rev: String,
    /// The hash of the source tree at that commit. Two commits with identical
    /// contents share one, which is what makes it a *content* check rather
    /// than a history check.
    pub tree: String,
}

/// Make `url` at `reference` available on disk.
///
/// `pinned` is the commit a lockfile already recorded. When it is given and
/// already checked out, nothing is fetched at all -- that is the ordinary case
/// for a build of an unchanged project. When it is given and what `reference`
/// now names is a *different* commit, that is refused: see the module note.
pub fn ensure(
    cache: &Path,
    url: &str,
    reference: &GitRef,
    pinned: Option<&str>,
    net: Net,
) -> Result<Checkout, String> {
    // A requirement names a release rather than a reference. Resolving it here
    // is for whoever asks for one directly -- `meadow add`; a build decides
    // between the requirements it has first and asks for the release it chose.
    let resolved;
    // A version names whichever release is newest when it is resolved, so a
    // release tagged since is not a tag that moved: what is pinned is still
    // the release it was, and `meadow update` is how the newer one is taken.
    let by_version = matches!(reference, GitRef::Version(_));
    let reference = match reference {
        GitRef::Version(req) => {
            let have = releases(cache, url, net)?;
            let versions: Vec<crate::semver::Version> =
                have.iter().map(|(v, _)| v.clone()).collect();
            let Some(best) = req.best(&versions) else {
                return Err(format!("no release of `{url}` is {req}"));
            };
            let tag = have
                .iter()
                .find(|(v, _)| v == best)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| format!("v{best}"));
            resolved = GitRef::Tag(tag);
            &resolved
        }
        other => other,
    };
    // A lockfile records `rev` as a full commit id, and it becomes both a
    // directory name and a revision handed to git. One that is a path -- an
    // absolute one, or holding `..` -- would otherwise select any directory
    // on disk, so a tampered lockfile could point a build at code that is not
    // the commit it names.
    if let Some(rev) = pinned
        && !is_commit_id(rev)
    {
        return Err(format!(
            "Meadow.lock gives `{rev}` as the commit of `{url}`, which is not a \
             commit id; refusing it"
        ));
    }

    let root = cache;
    let slug = slug(url);
    let db = root.join("db").join(&slug);

    // The fast path: the commit is known and already unpacked.
    if let Some(rev) = pinned {
        let path = root.join("checkouts").join(&slug).join(rev);
        if path.is_dir() {
            let tree = tree_of(&db, rev).unwrap_or_default();
            return Ok(Checkout {
                path,
                rev: rev.to_string(),
                tree,
            });
        }
    }

    if net == Net::Offline {
        let what = match reference.written() {
            Some(r) => format!("{url} ({r})"),
            None => url.to_string(),
        };
        return Err(format!(
            "`{what}` is not in the cache, and this build is offline.\n\
             Run it without `--offline` once to fetch it."
        ));
    }

    fetch(&db, url, reference)?;

    let rev = match pinned {
        // A pinned commit is asked for by name: it need not still be what the
        // branch or tag points at, and asking for it directly is what lets an
        // old lockfile keep building.
        Some(rev) => {
            let found = rev_parse(&db, rev).map_err(|_| {
                format!(
                    "`{url}` has no commit {rev}, which Meadow.lock says to use.\n\
                     The history it was on may have been rewritten."
                )
            })?;
            // A branch or tag that has moved is reported rather than followed.
            if let Ok(now) = rev_parse(&db, reference.refspec())
                && now != found
                && !by_version
                && !matches!(reference, GitRef::Default | GitRef::Branch(_))
            {
                return Err(format!(
                    "`{}` of `{url}` now names commit {}, but Meadow.lock says {}.\n\
                     A tag that moves is a different program under the same name. \
                     Run `meadow update` to take the new one deliberately.",
                    reference.written().unwrap_or_else(|| "HEAD".to_string()),
                    short(&now),
                    short(&found)
                ));
            }
            found
        }
        None => rev_parse(&db, reference.refspec()).map_err(|e| {
            format!(
                "`{url}` has no {}: {e}",
                reference
                    .written()
                    .unwrap_or_else(|| "default branch".to_string())
            )
        })?,
    };

    let path = root.join("checkouts").join(&slug).join(&rev);
    if !path.is_dir() {
        unpack(&db, &rev, &path)?;
    }
    let tree = tree_of(&db, &rev)?;
    Ok(Checkout { path, rev, tree })
}

/// The commit `reference` names right now, without checking anything out.
/// What `meadow update` asks to decide whether there is something newer.
pub fn latest(cache: &Path, url: &str, reference: &GitRef, net: Net) -> Result<String, String> {
    if net == Net::Offline {
        return Err(format!("cannot look at `{url}` while offline"));
    }
    let db = cache.join("db").join(slug(url));
    fetch(&db, url, reference)?;
    rev_parse(&db, reference.refspec())
        .map_err(|e| format!("`{url}` has no {}: {e}", reference.refspec()))
}

/// Every release the repository has: a tag that reads as a version, with the
/// tag it was written as, newest last.
///
/// A tag that is not a version -- `nightly`, `v1.2` -- is not a release and is
/// passed over. Offline, what was fetched before is used: a build that has
/// resolved once can resolve again with no network.
pub fn releases(
    cache: &Path,
    url: &str,
    net: Net,
) -> Result<Vec<(crate::semver::Version, String)>, String> {
    let db = cache.join("db").join(slug(url));
    if net == Net::Offline {
        if !db.join("HEAD").is_file() {
            return Err(format!(
                "cannot look for the releases of `{url}` while offline"
            ));
        }
    }
    // Held until the tags are read, so that they are the tags of a fetch that
    // finished.
    let _held = DbLock::hold(&db);
    if net != Net::Offline {
        fetch_held(&db, url, &GitRef::Default)?;
    }
    let out = git(Some(&db), &["tag", "--list"])?;
    let mut found: Vec<(crate::semver::Version, String)> = out
        .lines()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .filter_map(|t| crate::semver::Version::parse(t).map(|v| (v, t.to_string())))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

// --- the git commands ---------------------------------------------------------

/// One build at a time in a repository's cache.
///
/// Two builds that want the same repository -- two `meadow`s, or two tests of
/// one run -- would otherwise both find it missing and both clone it, the
/// second clearing away what the first had half made; or one would list the
/// tags of a clone the other had begun and not finished, find none, and say
/// the repository has no releases. A file beside the cache is the lock: made
/// by whoever holds it, removed when they are done.
struct DbLock(std::path::PathBuf);

impl DbLock {
    fn hold(db: &Path) -> DbLock {
        let mut name = db.as_os_str().to_os_string();
        name.push(".lock");
        let path = std::path::PathBuf::from(name);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let begun = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Left by a build that was killed: nothing fetches for
                    // this long. And past the wait, go on without it rather
                    // than never build.
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > std::time::Duration::from_secs(600));
                    if stale || begun.elapsed() > std::time::Duration::from_secs(900) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                // Somewhere a file cannot be made: the fetch will say why.
                Err(_) => break,
            }
        }
        DbLock(path)
    }
}

impl Drop for DbLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Clone `url` into `db` if it is not there, then fetch into it.
fn fetch(db: &Path, url: &str, reference: &GitRef) -> Result<(), String> {
    let _held = DbLock::hold(db);
    fetch_held(db, url, reference)
}

/// [`fetch`], by whoever holds the cache's [`DbLock`].
fn fetch_held(db: &Path, url: &str, reference: &GitRef) -> Result<(), String> {
    // Before git is told to fetch it: a url that is really an option, or a
    // remote-helper command, would otherwise run at build time.
    safe_url(url)?;
    crate::status::status("Updating", format!("git repository `{url}`"));
    if !db.join("HEAD").is_file() {
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
        }
        // A partial clone left by an interrupted run would never complete.
        let _ = std::fs::remove_dir_all(db);
        // `--` so git reads `url` as a location, never as an option.
        git_fetching(
            None,
            &["clone", "--bare", "--", url, &db.display().to_string()],
        )?;
        return Ok(());
    }
    // `+` on both sides: a force-moved tag is brought over so that it can be
    // *reported* as moved rather than silently missed.
    let refs = [
        "+refs/heads/*:refs/heads/*".to_string(),
        "+refs/tags/*:refs/tags/*".to_string(),
    ];
    let mut args = vec!["fetch", "--force", "--prune", "--", url];
    args.extend(refs.iter().map(String::as_str));
    let _ = reference;
    git_fetching(Some(db), &args)
}

/// Run a `git` that transfers something, showing how far it has got.
///
/// With nobody watching -- a test, the language server -- this is just [`git`]
/// with `--quiet`. Otherwise `--progress` makes git report as it goes, even
/// though its stderr is a pipe, and those reports drive the fetch bar.
fn git_fetching(dir: Option<&Path>, args: &[&str]) -> Result<(), String> {
    let quiet = !crate::status::enabled();
    let mut all: Vec<&str> = vec![args[0], if quiet { "--quiet" } else { "--progress" }];
    all.extend_from_slice(&args[1..]);
    if quiet {
        return git(dir, &all).map(|_| ());
    }

    let mut cmd = git_command(dir);
    cmd.args(&all)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "`git` is not installed, and a git dependency needs it".to_string()
        } else {
            format!("could not run git: {e}")
        }
    })?;

    // git ends a progress line with `\r` and redraws it, and ends every other
    // line with `\n`; either finishes a line here.
    let mut said = String::new();
    let mut bar = crate::status::Fetching::new();
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while matches!(err.read(&mut byte), Ok(1)) {
            if byte[0] == b'\r' || byte[0] == b'\n' {
                let text = String::from_utf8_lossy(&line).into_owned();
                if let Some((percent, rate)) = crate::status::git_progress(&text) {
                    bar.update(percent, rate.as_deref());
                } else if !text.trim().is_empty() {
                    said.push_str(&text);
                    said.push('\n');
                }
                line.clear();
            } else {
                line.push(byte[0]);
            }
        }
    }
    drop(bar);
    let status = child
        .wait()
        .map_err(|e| format!("could not wait for git: {e}"))?;
    if status.success() {
        return Ok(());
    }
    // Only what explains the failure: git's own chatter about what it was
    // doing is not the reason it stopped.
    let reason: Vec<&str> = said
        .lines()
        .filter(|l| {
            let l = l.trim_start();
            l.starts_with("fatal:") || l.starts_with("error:") || l.starts_with("remote: ")
        })
        .collect();
    Err(if reason.is_empty() {
        format!("git {} failed", args[0])
    } else {
        reason.join("\n")
    })
}

/// The commit `what` names, in full.
fn rev_parse(db: &Path, what: &str) -> Result<String, String> {
    // `^{commit}` so that an annotated tag gives the commit it points at rather
    // than the tag object, which is what a checkout needs.
    let out = git(
        Some(db),
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{what}^{{commit}}"),
        ],
    )?;
    Ok(out.trim().to_string())
}

/// The hash of the source tree at `rev`.
fn tree_of(db: &Path, rev: &str) -> Result<String, String> {
    let out = git(
        Some(db),
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{rev}^{{tree}}"),
        ],
    )?;
    Ok(out.trim().to_string())
}

/// Put the source at `rev` into `path`.
///
/// Written beside where it belongs and then moved, so that an interrupted run
/// cannot leave a half-written checkout that later builds would trust.
fn unpack(db: &Path, rev: &str, path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
    // This unpacking's own: two builds unpacking the same commit at once --
    // a test's and the compiler's it runs beside, say, or two threads of one
    // -- would otherwise write over, and remove, each other's.
    static UNPACKED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nth = UNPACKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = parent.join(format!(".{rev}.{}-{nth}.part", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);

    // `--local` hardlinks the objects instead of copying them, so a checkout
    // costs about what its working tree costs.
    git(
        None,
        &[
            "clone",
            "--quiet",
            "--local",
            "--no-checkout",
            &db.display().to_string(),
            &staging.display().to_string(),
        ],
    )?;
    git(Some(&staging), &["checkout", "--quiet", "--detach", rev])?;

    // A few times: on Windows a directory just written may be held a moment
    // by whatever scans new files, and the move is refused while it is.
    let mut tries = 0;
    loop {
        match std::fs::rename(&staging, path) {
            Ok(()) => return Ok(()),
            // Another build may have unpacked the same commit first, which is
            // fine: the contents are the same by construction.
            Err(_) if path.is_dir() => {
                let _ = std::fs::remove_dir_all(&staging);
                return Ok(());
            }
            Err(_) if tries < 10 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(format!("could not put {} in place: {e}", path.display()));
            }
        }
    }
}

/// Run `git`, answering its output or what it said went wrong.
/// A `git` told what it needs to be told before anything else.
///
/// `core.longpaths` is the one that matters, and only on Windows: git there
/// refuses a path over 260 characters -- "Filename too long" -- and a
/// dependency's cache sits under a package's own directory, which on a build
/// machine is already deep. Set for the invocation rather than in anyone's
/// configuration: it is this build's business, not theirs.
fn git_command(dir: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    // The `ext::` transport runs an arbitrary command; a dependency url is not
    // trusted to ask for that, whatever the user's git configuration allows.
    cmd.args(["-c", "protocol.ext.allow=never"]);
    if cfg!(windows) {
        cmd.args(["-c", "core.longpaths=true"]);
    }
    // `-C`, not `--git-dir`: this runs against the bare cache *and* against a
    // checkout, whose git directory is `.git` inside it rather than the
    // directory itself. Letting git find the repository handles both.
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    cmd
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut cmd = git_command(dir);
    cmd.args(args);
    // Never stop for a password prompt: a build that hangs waiting for input
    // nobody is watching is worse than one that fails.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "`git` is not installed, and a git dependency needs it".to_string()
        } else {
            format!("could not run git: {e}")
        }
    })?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let said = said.trim();
        return Err(if said.is_empty() {
            format!("git {} failed", args.first().unwrap_or(&""))
        } else {
            said.to_string()
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// --- where things are kept ----------------------------------------------------

/// Where fetched repositories are kept when nothing says otherwise:
/// `$MEADOW_HOME/git`.
///
/// Passed in rather than looked up inside [`ensure`], so that a test can fetch
/// into a directory of its own without setting an environment variable that
/// every other part of the build also reads.
pub fn default_cache() -> Result<PathBuf, String> {
    crate::stdlib::home()
        .map(|h| h.join("git"))
        .ok_or_else(|| "no home directory to cache git dependencies in".to_string())
}

/// `url` with the endings that do not change where it points removed, so that
/// the spellings of one repository are one repository.
fn normalise(url: &str) -> &str {
    url.trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/')
}

/// A directory name for `url`: readable, and unique.
///
/// The last path segment makes it recognisable when looking through the cache;
/// the hash of the whole URL is what actually keeps two repositories of the
/// same name apart. Both are taken from the normalised URL, so a trailing `/`
/// or `.git` does not make a second copy of what is already here.
fn slug(url: &str) -> String {
    let url = normalise(url);
    let name = url
        .rsplit(['/', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or("repo");
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut h = std::hash::DefaultHasher::new();
    std::hash::Hasher::write(&mut h, url.as_bytes());
    format!("{name}-{:016x}", std::hash::Hasher::finish(&h))
}

/// A commit, short enough to read.
pub fn short(rev: &str) -> &str {
    &rev[..rev.len().min(9)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_is_readable_and_keeps_two_urls_apart() {
        let a = slug("https://github.com/someone/meadow-json");
        let b = slug("https://github.com/other/meadow-json");
        assert!(a.starts_with("meadow-json-"), "{a}");
        assert!(b.starts_with("meadow-json-"), "{b}");
        assert_ne!(a, b, "two repositories of one name must not share a slug");
    }

    #[test]
    fn a_slug_ignores_the_spellings_that_mean_one_repository() {
        // A trailing slash or `.git` names the same place, and should not make
        // a second copy in the cache.
        let plain = slug("https://github.com/someone/meadow-json");
        assert_eq!(slug("https://github.com/someone/meadow-json/"), plain);
        assert_eq!(slug("https://github.com/someone/meadow-json.git"), plain);
    }

    #[test]
    fn an_ssh_url_still_gets_a_name() {
        let s = slug("git@github.com:someone/meadow-json.git");
        assert!(s.starts_with("meadow-json-"), "{s}");
    }

    #[test]
    fn a_slug_has_nothing_in_it_a_path_would_mind() {
        let s = slug("https://example.com/a b/c?d=e#f");
        assert!(
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{s}"
        );
    }
}

#[cfg(test)]
mod live {
    use super::*;
    use crate::package::GitRef;

    /// A repository made on the spot, so these need no network.
    fn repo(which: &str) -> Option<meadow_scratch::Scratch> {
        let dir = meadow_scratch::scratch!("meadow-git-src-{}-{which}", std::process::id());
        std::fs::create_dir_all(dir.join("src")).ok()?;
        std::fs::write(
            dir.join("Meadow.toml"),
            "[package]\nname = \"greet\"\nversion = \"0.1.0\"\n",
        )
        .ok()?;
        std::fs::write(dir.join("src/Lib.mw"), "@pub fun greeting = \"hi\"\n").ok()?;
        let d = dir.display().to_string();
        for args in [
            vec!["init", "--quiet", "-b", "main", &d],
            vec!["-C", &d, "add", "-A"],
            vec![
                "-C",
                &d,
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--quiet",
                "-m",
                "first",
            ],
            vec!["-C", &d, "tag", "v1.0.0"],
        ] {
            let ok = Command::new("git")
                .args(&args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !ok {
                return None;
            }
        }
        Some(dir)
    }

    /// An empty cache directory, removed when the test is done. Naming one stands for a machine: two different
    /// names are two machines that share a lockfile and nothing else.
    ///
    /// A directory rather than `MEADOW_HOME`, so these run beside anything
    /// else: that variable also says where the runtime library and the
    /// extracted standard library live, and setting it would move those too.
    fn cache(which: &str) -> meadow_scratch::Scratch {
        meadow_scratch::scratch!("meadow-git-cache-{}-{which}", std::process::id())
    }

    /// Run `git` in `dir`, asserting it worked.
    fn run(dir: &str, args: &[&str]) {
        let mut all = vec!["-C", dir];
        all.extend_from_slice(args);
        let out = Command::new("git").args(&all).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    #[test]
    fn a_tag_is_fetched_checked_out_and_pinned() {
        let Some(src) = repo("one") else {
            eprintln!("skipped: no usable git");
            return;
        };
        let url = src.display().to_string();
        let cache = cache("one");
        let tag = GitRef::Tag("v1.0.0".into());

        let got = ensure(&cache, &url, &tag, None, Net::Allowed).expect("the tag is there");
        assert!(
            got.path.join("Meadow.toml").is_file(),
            "no manifest in the checkout"
        );
        assert_eq!(got.rev.len(), 40, "a full commit name");
        // The tree hash itself, and nothing else: `rev-parse` echoes an option
        // it does not take, and once wrote `--end-of-options` into lockfiles.
        let want = Command::new("git")
            .args(["-C", &url, "rev-parse", &format!("{}^{{tree}}", got.rev)])
            .output()
            .expect("git runs");
        assert_eq!(
            got.tree,
            String::from_utf8_lossy(&want.stdout).trim(),
            "the tree"
        );
        assert!(
            got.tree.len() == 40 && got.tree.chars().all(|c| c.is_ascii_hexdigit()),
            "a tree hash and only that: {:?}",
            got.tree
        );

        // Asking again with the commit pinned must not need the network.
        let again =
            ensure(&cache, &url, &tag, Some(&got.rev), Net::Offline).expect("already cached");
        assert_eq!(again.rev, got.rev);
        assert_eq!(again.path, got.path);
    }

    #[test]
    fn a_tag_that_moved_is_refused_rather_than_followed() {
        let Some(src) = repo("moved") else {
            eprintln!("skipped: no usable git");
            return;
        };
        let url = src.display().to_string();
        let d = src.display().to_string();
        let tag = GitRef::Tag("v1.0.0".into());

        // One machine locks the tag.
        let first =
            ensure(&cache("before"), &url, &tag, None, Net::Allowed).expect("the tag is there");

        // The tag is then moved, as a force-push would move it.
        std::fs::write(
            src.join("src/Lib.mw"),
            "@pub fun greeting = \"different\"\n",
        )
        .unwrap();
        run(&d, &["add", "-A"]);
        run(
            &d,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--quiet",
                "-m",
                "second",
            ],
        );
        run(&d, &["tag", "-f", "v1.0.0"]);

        // Another machine builds from that lockfile with nothing cached -- what
        // CI does. It has to fetch, and what it fetches is not what was locked.
        //
        // A warm cache would not notice, and should not: the lock says take
        // that commit, and taking it is what it already has.
        let err = ensure(&cache("after"), &url, &tag, Some(&first.rev), Net::Allowed)
            .expect_err("a moved tag must not be followed silently");
        assert!(err.contains("now names commit"), "{err}");
    }

    #[test]
    fn being_offline_says_what_it_would_have_fetched() {
        let err = ensure(
            &cache("offline"),
            "https://example.invalid/nothing",
            &GitRef::Tag("v9".into()),
            None,
            Net::Offline,
        )
        .expect_err("offline and not cached");
        assert!(err.contains("offline"), "{err}");
        assert!(err.contains("tag=v9"), "{err}");
    }
}
