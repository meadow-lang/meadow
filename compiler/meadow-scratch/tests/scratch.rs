//! A scratch directory is gone when its test is over, however the test ended.

use meadow_scratch::{KEEP, Scratch, scratch};
use std::path::PathBuf;
use std::process::Command;

/// What the suites' helpers do: a package written into a directory of its own.
fn package(what: &str) -> Scratch {
    let dir = scratch!("meadow-scratch-{}-{what}", std::process::id());
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Meadow.toml"), "[package]\nname = \"P\"\n").unwrap();
    std::fs::write(dir.join("src/Lib.mw"), "def result = 1\n").unwrap();
    dir
}

#[test]
fn a_directory_is_removed_when_its_test_is_done() {
    let path = {
        let dir = package("done");
        assert!(dir.join("src/Lib.mw").is_file());
        dir.to_path_buf()
    };
    assert!(!path.exists(), "{} is still there", path.display());
}

#[test]
fn a_directory_is_removed_when_its_test_panics() {
    let seen = std::sync::Mutex::new(PathBuf::new());
    // As the test harness runs a test: on a thread of its own, which the
    // panic ends.
    let failed = std::thread::scope(|s| {
        s.spawn(|| {
            let dir = package("panics");
            *seen.lock().unwrap() = dir.to_path_buf();
            panic!("a deliberate failure");
        })
        .join()
        .is_err()
    });
    assert!(failed);
    let path = seen.into_inner().unwrap();
    assert!(path.starts_with(meadow_scratch::root(option_env!("CARGO_TARGET_TMPDIR"))));
    assert!(!path.exists(), "{} is still there", path.display());
}

#[test]
fn it_is_under_the_target_directory() {
    let dir = package("where");
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    assert!(
        dir.starts_with(&tmp),
        "{} is not in {}",
        dir.display(),
        tmp.display()
    );
}

#[test]
fn what_an_earlier_run_left_is_cleared_first() {
    let first = package("again").keep();
    std::fs::write(first.join("left-over"), "from before").unwrap();
    let second = package("again");
    assert_eq!(second.path(), first);
    assert!(!second.join("left-over").exists());
    assert!(second.join("src/Lib.mw").is_file());
}

#[test]
fn kept_is_kept() {
    let path = package("kept").keep();
    assert!(path.is_dir());
    meadow_scratch::remove(&path);
    assert!(!path.exists());
}

#[test]
fn a_child_and_what_is_held_go_with_it() {
    let (parent, child, held) = {
        let dep = package("dep");
        let app = scratch!("meadow-scratch-{}-app", std::process::id()).child("app");
        let paths = (
            app.parent().unwrap().to_path_buf(),
            app.to_path_buf(),
            dep.to_path_buf(),
        );
        let app = app.holding(dep);
        assert!(app.is_dir() && paths.2.is_dir());
        paths
    };
    for p in [parent, child, held] {
        assert!(!p.exists(), "{} is still there", p.display());
    }
}

/// Run by `asked_for_it_is_kept_after_a_failure`, in a process of its own,
/// with `MEADOW_KEEP_TEST_DIRS` set or not.
#[test]
#[ignore = "run by asked_for_it_is_kept_after_a_failure"]
fn fails_holding_a_directory() {
    let parent = std::env::var("MEADOW_SCRATCH_PARENT").unwrap_or_default();
    let dir = package(&format!("asked-{parent}"));
    std::fs::write(dir.join("evidence"), "what failed").unwrap();
    panic!("a deliberate failure");
}

#[test]
fn asked_for_it_is_kept_after_a_failure() {
    let me = std::process::id();
    let run = |keep: &str| {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "fails_holding_a_directory"])
            .env(KEEP, keep)
            .env("MEADOW_SCRATCH_PARENT", me.to_string())
            .output()
            .unwrap();
        assert!(!out.status.success(), "the deliberate failure passed");
        // The harness shows what a failing test printed on its stdout.
        let said = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        // The child names the directory by its own process and by ours.
        let left: Vec<PathBuf> =
            std::fs::read_dir(meadow_scratch::root(option_env!("CARGO_TARGET_TMPDIR")))
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(&format!("-asked-{me}")))
                })
                .collect();
        (said, left)
    };

    let (said, left) = run("1");
    assert_eq!(left.len(), 1, "{said}");
    assert!(left[0].join("evidence").is_file(), "{said}");
    assert!(said.contains("kept"), "{said}");
    meadow_scratch::remove(&left[0]);

    let (said, left) = run("");
    assert!(left.is_empty(), "the failure's directory was left: {said}");
    assert!(
        said.contains(KEEP),
        "a failure does not say how to keep it: {said}"
    );
}
