//! The toolchain's caches under `~/.meadow/lib`, and keeping them from filling
//! the disk.
//!
//! Two kinds of thing are kept there, each in a directory named for the
//! sources that made it:
//!
//! * `meadow_glade_<hash>/` and `meadow_silo_<hash>/`: a runtime library a
//!   `meadow` carries built in, written out the first time a link wants it
//!   (`aot::embedded`). Older binaries wrote `meadow_rts_<hash>/`.
//! * `std/<fingerprint>/`: the standard library, compiled by a `meadow` that
//!   was built without it precompiled (`stdlib::cached`).
//!
//! The hash covers the sources, so each `meadow` built from different ones --
//! every nightly, every development build -- writes its own, and none of them
//! is any use to a `meadow` built from other sources. Nothing ever needs an old
//! one again once the binary that wrote it is gone, and at tens of megabytes
//! apiece they fill a disk within weeks of development. So they are pruned.
//!
//! **The rule.** Only a directory whose name has one of those shapes is ever
//! removed; anything else in `lib` is not this module's to judge. Of those:
//!
//! * this binary's own are kept;
//! * so is any used within the last week ([`KEEP_FOR`]), since another
//!   `meadow` on the machine -- a development build, the toolchain `meadowup`
//!   installed -- may still want it. "Used" means a link or a build last read
//!   it, which each `meadow` stamps as it does (`.used`), not when it was
//!   written;
//! * nothing used within the last hour ([`IN_USE`]) is removed by anything,
//!   the manual clean included, since a link may be reading it at this moment.
//!
//! Removing one too many costs one re-extraction (or one compile of `Std`) by
//! the `meadow` that wants it again; keeping too many costs a full disk, and a
//! full disk breaks every link on the machine. So the rule leans towards
//! removing.
//!
//! **When.** [`auto`] prunes at most once a day (the stamp is `lib/.pruned`),
//! the first time in a run that `meadow` reaches for the cache, and before it
//! writes anything there -- so a disk the cache has filled gets the room back
//! first. `meadow clean --cache` ([`clean`]) prunes now, keeping only this
//! binary's own (and what is in use).
//!
//! Every failure is ignored. Another `meadow` may be writing or removing the
//! same directories, and on Windows a library a linker has open cannot be
//! deleted; a directory that cannot be removed today is tried again tomorrow,
//! and no build ever fails because a cache could not be pruned.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long a cache directory another `meadow` may want is kept after it was
/// last used.
pub const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How recently used a directory has to be for nothing to remove it: a link
/// may be reading it.
pub const IN_USE: Duration = Duration::from_secs(60 * 60);

/// How often [`auto`] prunes.
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The file a `meadow` rewrites in a cache directory when it uses it.
const USED: &str = ".used";

/// The file in `lib` whose age says when [`auto`] last pruned.
const PRUNED: &str = ".pruned";

/// `~/.meadow/lib`, or under `MEADOW_HOME`.
pub fn lib_dir() -> Option<PathBuf> {
    Some(crate::stdlib::home()?.join("lib"))
}

/// This binary's own cache directories, relative to [`lib_dir`] -- whether or
/// not it has written them.
pub fn current() -> Vec<PathBuf> {
    vec![
        PathBuf::from(crate::aot::Runtime::Glade.symbol()),
        PathBuf::from(crate::aot::Runtime::Silo.symbol()),
        Path::new("std").join(crate::stdlib::fingerprint()),
    ]
}

/// Note that `dir`, a cache directory, is being used now, so that no prune
/// takes it for stale. Best effort, like everything here.
pub fn used(dir: &Path) {
    let _ = std::fs::write(dir.join(USED), b"");
}

/// What a prune did, for the caller to report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// The directories removed, or that would be.
    pub removed: Vec<PathBuf>,
    /// How many bytes that freed, or would.
    pub bytes: u64,
    /// Other `meadow`s' cache directories, left because they were used too
    /// recently. (This binary's own are always left, and not listed.)
    pub recent: Vec<PathBuf>,
    /// Stale ones that could not be removed, or not wholly: something had a
    /// file open, or another `meadow` was writing there.
    pub failed: Vec<PathBuf>,
}

/// Remove from `lib` every cache directory not in `keep` (paths relative to
/// `lib`, as [`current`] gives them) and not used for `unused_for` -- which is
/// never less than [`IN_USE`] -- as of `now`. `dry_run` finds and measures
/// without removing.
pub fn sweep(
    lib: &Path,
    keep: &[PathBuf],
    unused_for: Duration,
    now: SystemTime,
    dry_run: bool,
) -> Pruned {
    let unused_for = unused_for.max(IN_USE);
    let mut out = Pruned::default();
    for rel in entries(lib) {
        if keep.contains(&rel) {
            continue;
        }
        let dir = lib.join(&rel);
        // Judged one at a time, just before it would go, so that a directory
        // another `meadow` started using while this ran is seen as used.
        let stale = last_used(&dir)
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= unused_for);
        if !stale {
            out.recent.push(dir);
            continue;
        }
        let size = crate::clean::size_of(&dir);
        if dry_run {
            out.bytes += size;
            out.removed.push(dir);
            continue;
        }
        let _ = std::fs::remove_dir_all(&dir);
        if dir.exists() {
            out.bytes += size.saturating_sub(crate::clean::size_of(&dir));
            out.failed.push(dir);
        } else {
            out.bytes += size;
            out.removed.push(dir);
        }
    }
    out
}

/// Prune by the rule in the module docs, if nothing has in the last day.
/// Cheap when it has: one look at a file's age, once per run.
pub fn auto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(lib) = lib_dir() {
            prune_if_due(&lib, SystemTime::now());
        }
    });
}

/// [`auto`]'s work, for `lib` and `now`: prune unless the stamp says it was
/// done within the day. Answers whether it pruned.
pub fn prune_if_due(lib: &Path, now: SystemTime) -> bool {
    let stamp = lib.join(PRUNED);
    let recent = std::fs::metadata(&stamp)
        .ok()
        .filter(|m| m.is_file())
        .and_then(|m| m.modified().ok())
        .and_then(|t| now.duration_since(t).ok())
        .is_some_and(|age| age < EVERY);
    if recent || !lib.is_dir() {
        return false;
    }
    // Claimed before it is done, so that a `meadow` starting meanwhile does
    // not do it as well. If the stamp cannot be written the prune still
    // happens, and simply happens again next time.
    let _ = std::fs::write(&stamp, b"");
    sweep(lib, &current(), KEEP_FOR, now, false);
    true
}

/// `meadow clean --cache`: remove every cache directory but this binary's own
/// now, rather than when they have gone unused for a week. What is in use is
/// still left -- see [`IN_USE`].
pub fn clean(dry_run: bool) -> Result<Pruned, String> {
    let lib =
        lib_dir().ok_or("there is no home directory to find the cache in: set MEADOW_HOME")?;
    Ok(sweep(&lib, &current(), IN_USE, SystemTime::now(), dry_run))
}

/// Every cache directory in `lib`, relative to it: the names this knows, and
/// nothing else. A symbolic link is not followed, nor judged.
fn entries(lib: &Path) -> Vec<PathBuf> {
    let dirs = |dir: &Path| -> Vec<String> {
        let Ok(read) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        read.flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    };
    let mut out: Vec<PathBuf> = dirs(lib)
        .into_iter()
        .filter(|name| is_runtime(name))
        .map(PathBuf::from)
        .collect();
    out.extend(
        dirs(&lib.join("std"))
            .into_iter()
            .filter(|name| is_hash(name))
            .map(|name| Path::new("std").join(name)),
    );
    out.sort();
    out
}

/// A runtime library's directory: `meadow_glade_`, `meadow_silo_` or the old
/// `meadow_rts_`, then its hash.
fn is_runtime(name: &str) -> bool {
    ["meadow_glade_", "meadow_silo_", "meadow_rts_"]
        .iter()
        .any(|prefix| name.strip_prefix(prefix).is_some_and(is_hash))
}

/// Sixteen lowercase hex digits: how every fingerprint here is written.
fn is_hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// When `dir` was last used: the newest of what is in it -- its library, its
/// `.used` stamp, a half-written file -- or, while it is empty, the directory
/// itself, which another `meadow` may have made a moment ago to write into.
/// `None` if it cannot be told, and then it is not removed.
fn last_used(dir: &Path) -> Option<SystemTime> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max();
    newest.or_else(|| std::fs::metadata(dir).ok()?.modified().ok())
}
