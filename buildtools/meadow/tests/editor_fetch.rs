//! The language server fetching a package's dependencies, as `meadow lsp
//! --fetch` does in a workspace the user trusts.
//!
//! A binary of its own: fetching is switched on for the whole process, and
//! `tests/editor.rs` checks that without it nothing is fetched. The
//! repository is made here, so no network is needed -- only a `git` on the
//! path, without which these skip.

use std::process::Command;

fn scratch(what: &str) -> meadow_scratch::Scratch {
    meadow_scratch::scratch!("meadow-fetch-{}-{what}", std::process::id())
}

fn git(args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A one-commit package tagged `v1.0.0`, or `None` with no usable git.
fn dependency_repo() -> Option<meadow_scratch::Scratch> {
    let dir = scratch("repo");
    std::fs::create_dir_all(dir.join("src")).ok()?;
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"Greet\"\nversion = \"0.1.0\"\n",
    )
    .ok()?;
    std::fs::write(dir.join("src/Lib.mw"), "@pub def greeting = \"hi\"\n").ok()?;
    let d = dir.display().to_string();
    let ok = git(&["init", "--quiet", "-b", "main", &d])
        && git(&["-C", &d, "add", "-A"])
        && git(&[
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
        ])
        && git(&["-C", &d, "tag", "v1.0.0"]);
    ok.then_some(dir)
}

fn app(what: &str, url: &str) -> meadow_scratch::Scratch {
    let dir = scratch(what);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\nGreet = {{ git = \"{url}\", tag = \"v1.0.0\" }}\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/Main.mw"), "def result = greeting\n").unwrap();
    dir
}

#[test]
fn the_server_fetches_what_a_package_needs_once_and_pins_it() {
    let Some(repo) = dependency_repo() else {
        eprintln!("skipped: no usable git");
        return;
    };
    // A cache of this binary's own, so nothing already fetched stands in.
    // Safety: set before anything in this process reads the environment
    // on another thread; this binary is these tests alone.
    let home = scratch("home");
    unsafe { std::env::set_var("MEADOW_HOME", home.as_os_str()) };
    meadow::editor::allow_fetching();

    let fetched = app("fetched", &repo.display().to_string());
    let file = fetched.join("src").join("Main.mw");
    let got = meadow::editor::find_package(&file).expect("the file is in a package");
    let sources = got.expect("fetched, and loaded");
    assert!(
        sources
            .deps
            .iter()
            .any(|(name, _)| name.to_string() == "Greet"),
        "the dependency came with it"
    );
    assert!(
        fetched.join("Meadow.lock").exists(),
        "pinned, as a build would have pinned it"
    );
    // From the cache from now on.
    assert!(meadow::editor::find_package(&file).expect("again").is_ok());

    // One that cannot be fetched says so, and is not tried on every edit.
    let nothing = scratch("nothing-here");
    let missing = app("missing", &nothing.display().to_string());
    let file = missing.join("src").join("Main.mw");
    let first = meadow::editor::find_package(&file).expect("in a package");
    let first = first.err().expect("nothing to fetch");
    assert!(first.contains("could not fetch"), "{first}");
    let again = meadow::editor::find_package(&file).expect("in a package");
    let again = again.err().expect("still nothing");
    assert!(again.contains("has not been fetched yet"), "{again}");
}
