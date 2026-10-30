//! The toolchain's cache under `~/.meadow/lib` (`meadow::cache`): what is
//! pruned and what is left, by the automatic prune and by
//! `meadow clean --cache`, and that a prune that fails is no build's problem.
//!
//! Every test works in a home of its own -- a scratch `lib` directory, or
//! `MEADOW_HOME` for the binary -- and ages a cache directory by setting the
//! times of the files in it.

use meadow::cache::{self, IN_USE, KEEP_FOR};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const HOUR: Duration = Duration::from_secs(60 * 60);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn scratch(who: &str) -> meadow_scratch::Scratch {
    meadow_scratch::scratch!("meadow-cache-{}-{who}", std::process::id())
}

/// Set `path`'s modification time to `age` ago.
fn age(path: &Path, age: Duration) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
}

/// A cache directory `lib/rel` holding a library of `bytes` bytes, last
/// written `ago`.
fn cache_dir(lib: &Path, rel: &str, bytes: usize, ago: Duration) -> PathBuf {
    let dir = lib.join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("meadow_glade.lib");
    std::fs::write(&file, vec![0u8; bytes]).unwrap();
    age(&file, ago);
    dir
}

/// A file at `lib/rel`, written `ago`.
fn file(lib: &Path, rel: &str, ago: Duration) -> PathBuf {
    let path = lib.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not the cache's").unwrap();
    age(&path, ago);
    path
}

/// This binary's own cache directories, made in `lib` and aged `ago`.
fn current(lib: &Path, ago: Duration) -> Vec<PathBuf> {
    cache::current()
        .iter()
        .map(|rel| cache_dir(lib, rel.to_str().unwrap(), 64, ago))
        .collect()
}

fn hash(n: u8) -> String {
    format!("{n:02x}").repeat(8)
}

#[test]
fn stale_directories_go_and_the_rest_stay() {
    let lib = scratch("rule");
    let long_ago = KEEP_FOR + DAY;
    let mine = current(&lib, long_ago * 4);
    let stale = [
        cache_dir(&lib, &format!("meadow_glade_{}", hash(1)), 100, long_ago),
        cache_dir(&lib, &format!("meadow_silo_{}", hash(2)), 200, long_ago),
        cache_dir(&lib, &format!("meadow_rts_{}", hash(3)), 300, long_ago * 3),
        cache_dir(&lib, &format!("std/{}", hash(4)), 400, long_ago),
    ];
    let recent = [
        cache_dir(&lib, &format!("meadow_silo_{}", hash(5)), 10, 2 * DAY),
        cache_dir(&lib, &format!("std/{}", hash(6)), 10, KEEP_FOR - HOUR),
    ];
    // Not the cache's, however old: a name of another shape, a file where a
    // directory would be, and whatever else has been put here.
    let strangers = [
        cache_dir(&lib, "meadow_glade_notahash", 10, long_ago),
        cache_dir(
            &lib,
            &format!("meadow_glade_{}", hash(0xab).to_uppercase()),
            10,
            long_ago,
        ),
        cache_dir(&lib, &format!("meadow_cut_{}", hash(8)), 10, long_ago),
        cache_dir(&lib, "mine", 10, long_ago),
        cache_dir(&lib, "std/not-a-fingerprint", 10, long_ago),
        file(&lib, &format!("meadow_silo_{}", hash(9)), long_ago),
        file(&lib, &format!("std/{}", hash(10)), long_ago),
        file(&lib, "notes.txt", long_ago),
    ];

    let out = cache::sweep(&lib, &cache::current(), KEEP_FOR, SystemTime::now(), false);

    for dir in &stale {
        assert!(
            !dir.exists(),
            "{} was stale, and is still there",
            dir.display()
        );
    }
    assert_eq!(out.removed.len(), stale.len(), "{out:?}");
    assert_eq!(out.bytes, 1000, "{out:?}");
    assert!(out.failed.is_empty(), "{out:?}");
    for dir in mine.iter().chain(&recent).chain(&strangers) {
        assert!(dir.exists(), "{} was removed", dir.display());
    }
    let mut kept = out.recent.clone();
    kept.sort();
    assert_eq!(kept, recent.to_vec());
}

#[test]
fn a_directory_in_use_is_kept_however_old_its_library() {
    let lib = scratch("in-use");
    let old = cache_dir(&lib, &format!("meadow_silo_{}", hash(1)), 10, KEEP_FOR * 2);
    // Another `meadow` linked against it a few minutes ago.
    let linked = cache_dir(&lib, &format!("meadow_glade_{}", hash(2)), 10, KEEP_FOR * 2);
    cache::used(&linked);
    // And one is writing this one now: nothing but its half-written library.
    let writing = lib.join(format!("meadow_glade_{}", hash(3)));
    std::fs::create_dir_all(&writing).unwrap();
    std::fs::write(writing.join("meadow_glade.lib.4242"), b"half").unwrap();
    // Just made, to be written into.
    let empty = lib.join(format!("std/{}", hash(4)));
    std::fs::create_dir_all(&empty).unwrap();

    // Asking for less than an hour is asking for an hour.
    let out = cache::sweep(&lib, &[], Duration::ZERO, SystemTime::now(), false);

    assert!(!old.exists(), "{out:?}");
    for dir in [&linked, &writing, &empty] {
        assert!(
            dir.exists(),
            "{} was in use, and was removed",
            dir.display()
        );
    }
    assert_eq!(out.recent.len(), 3, "{out:?}");
    // The hour is from the last use, so after one it is fair game.
    let later = SystemTime::now() + IN_USE + HOUR;
    let out = cache::sweep(&lib, &[], Duration::ZERO, later, false);
    assert_eq!(out.removed.len(), 3, "{out:?}");
}

#[test]
fn the_automatic_prune_runs_once_a_day() {
    let lib = scratch("daily");
    let first = cache_dir(&lib, &format!("meadow_silo_{}", hash(1)), 10, KEEP_FOR * 2);
    let now = SystemTime::now();
    assert!(cache::prune_if_due(&lib, now));
    assert!(!first.exists());

    let second = cache_dir(&lib, &format!("meadow_silo_{}", hash(2)), 10, KEEP_FOR * 2);
    assert!(
        !cache::prune_if_due(&lib, now + HOUR),
        "pruned twice in a day"
    );
    assert!(second.exists());
    assert!(cache::prune_if_due(&lib, now + DAY + HOUR));
    assert!(!second.exists());
}

/// A stale directory that cannot be removed: one holding a file this process
/// has open with no sharing, on Windows; one holding a directory that may not
/// be written to, elsewhere.
struct Stuck {
    dir: PathBuf,
    #[cfg(windows)]
    _held: std::fs::File,
}

fn stuck(lib: &Path, rel: &str) -> Stuck {
    let dir = cache_dir(lib, rel, 10, KEEP_FOR * 2);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let path = dir.join("meadow_glade.lib");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        Stuck { dir, _held: held }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        let inner = locked.join("x");
        std::fs::write(&inner, b"x").unwrap();
        age(&inner, KEEP_FOR * 2);
        // Writing in it made the directory new, and new is in use.
        std::fs::File::open(&locked)
            .unwrap()
            .set_modified(SystemTime::now() - KEEP_FOR * 2)
            .unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        Stuck { dir }
    }
}

impl Drop for Stuck {
    fn drop(&mut self) {
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                self.dir.join("locked"),
                std::fs::Permissions::from_mode(0o755),
            );
        }
    }
}

#[test]
fn what_cannot_be_removed_is_left_and_the_rest_still_goes() {
    let lib = scratch("stuck");
    let stuck = stuck(&lib, &format!("meadow_glade_{}", hash(1)));
    let other = cache_dir(&lib, &format!("meadow_silo_{}", hash(2)), 10, KEEP_FOR * 2);

    let out = cache::sweep(&lib, &[], KEEP_FOR, SystemTime::now(), false);

    assert!(!other.exists(), "{out:?}");
    // Run as root, nothing is stuck; otherwise it is reported, not an error.
    if stuck.dir.exists() {
        assert_eq!(out.failed, vec![stuck.dir.clone()], "{out:?}");
    }
}

fn meadow(dir: &Path, home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(dir)
        .env("MEADOW_HOME", home)
        .args(args)
        .output()
        .expect("the meadow binary runs");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn clean_cache_removes_all_but_this_meadows_own_and_says_how_much() {
    let home = scratch("clean");
    let lib = home.join("lib");
    let mine = current(&lib, KEEP_FOR * 2);
    let others = [
        cache_dir(
            &lib,
            &format!("meadow_glade_{}", hash(1)),
            1 << 20,
            2 * HOUR,
        ),
        cache_dir(&lib, &format!("meadow_silo_{}", hash(2)), 1 << 20, 2 * HOUR),
        cache_dir(&lib, &format!("std/{}", hash(3)), 1 << 20, 2 * HOUR),
    ];
    let busy = cache_dir(&lib, &format!("meadow_silo_{}", hash(4)), 10, KEEP_FOR * 2);
    cache::used(&busy);
    let stranger = file(&lib, "notes.txt", KEEP_FOR * 2);

    let (ok, out) = meadow(&home, &home, &["clean", "--cache", "--dry-run"]);
    assert!(ok, "{out}");
    assert!(out.contains("Would remove 3 cache directories"), "{out}");
    assert!(
        others.iter().all(|d| d.exists()),
        "a dry run removed something"
    );

    let (ok, out) = meadow(&home, &home, &["clean", "--cache"]);
    assert!(ok, "{out}");
    assert!(out.contains("Removed 3 cache directories"), "{out}");
    assert!(out.contains("3.0 MiB freed"), "{out}");
    assert!(out.contains("Kept 1 used within the last hour"), "{out}");
    assert!(others.iter().all(|d| !d.exists()), "{out}");
    for dir in mine.iter().chain([&busy, &stranger]) {
        assert!(dir.exists(), "{} was removed\n{out}", dir.display());
    }

    let (ok, out) = meadow(&home, &home, &["clean", "--cache"]);
    assert!(ok, "{out}");
    assert!(out.contains("nothing to remove"), "{out}");
}

/// A build prunes the cache as it goes, and a prune that cannot do what it
/// would -- its stamp unwritable, a directory held open -- changes nothing
/// about the build.
#[test]
fn a_build_prunes_and_a_failed_prune_does_not_fail_it() {
    let home = scratch("build");
    let lib = home.join("lib");
    let stale = cache_dir(&lib, &format!("meadow_silo_{}", hash(1)), 10, KEEP_FOR * 2);
    let stuck = stuck(&lib, &format!("std/{}", hash(2)));
    // Where the stamp would go, a directory, so it cannot be written.
    std::fs::create_dir_all(lib.join(".pruned")).unwrap();

    let pkg = home.join("Hello");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(pkg.join("Meadow.toml"), "[package]\nname = \"Hello\"\n").unwrap();
    std::fs::write(pkg.join("src/Main.mw"), "fun main () = println \"hi\"\n").unwrap();

    let (ok, out) = meadow(&pkg, &home, &["run", "."]);
    assert!(ok, "{out}");
    assert!(out.contains("hi"), "{out}");
    assert!(!stale.exists(), "the build did not prune\n{out}");
    drop(stuck);
}
