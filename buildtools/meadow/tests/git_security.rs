//! A git dependency's url, ref and pinned commit reach the `git` CLI, so a
//! manifest or lockfile is attacker-controlled input. None of it may be read
//! by git as an option or a command, and a pinned commit may not be a path.
//!
//! These need no network; the injection tests never let git run at all.

use meadow::git::{self, Net};
use meadow::package::{GitRef, Resolver};
use meadow::pipeline;

fn scratch(what: &str) -> meadow_scratch::Scratch {
    meadow_scratch::scratch!("meadow-gitsec-{}-{what}", std::process::id())
}

// --- the url allowlist -------------------------------------------------------

#[test]
fn a_url_that_git_would_read_as_an_option_is_refused() {
    // `--upload-pack=<cmd>` runs `<cmd>`; any leading `-` is an option.
    for url in [
        "--upload-pack=touch x",
        "-oProxyCommand=touch x",
        "--config=core.fsmonitor=touch x",
        "-",
    ] {
        assert!(
            git::safe_url(url).is_err(),
            "`{url}` should be refused as an option"
        );
    }
}

#[test]
fn a_remote_helper_transport_is_refused() {
    // `ext::` and friends run a command through git's remote-helper machinery.
    for url in ["ext::sh -c touch% x", "fd::17", "helper::whatever"] {
        assert!(
            git::safe_url(url).is_err(),
            "`{url}` should be refused as a transport helper"
        );
    }
}

#[test]
fn an_ordinary_url_is_allowed() {
    for url in [
        "https://github.com/someone/pkg",
        "http://example.com/pkg.git",
        "ssh://git@github.com/someone/pkg",
        "git://example.com/pkg.git",
        "file:///home/me/pkg",
        "git@github.com:someone/pkg",
        "user@host.example:some/path",
        // A repository on this machine: what CI's tests fetch from, and what
        // the first version of this check refused on Unix.
        "/tmp/meadow-git-src/repo",
        "./vendor/repo",
        "../repo",
        "C:\\Users\\me\\repo",
        "C:/Users/me/repo",
    ] {
        assert!(git::safe_url(url).is_ok(), "`{url}` should be allowed");
    }
    assert!(git::safe_url("").is_err(), "an empty url is not a location");
}

// --- the pinned commit is not a path ----------------------------------------

#[test]
fn a_pinned_rev_that_is_a_path_is_refused() {
    let cache = scratch("pin-path");
    // A lockfile could name any of these; each would otherwise become a
    // directory the checkout is read from.
    for bad in [
        "../../evil",
        "..",
        "/etc/passwd",
        "C:/Windows",
        "0123456789abcdef", // too short to be a commit id
        "main",
    ] {
        let e = git::ensure(
            &cache,
            "https://example.com/x",
            &GitRef::Default,
            Some(bad),
            Net::Offline,
        )
        .expect_err(&format!("`{bad}` is not a commit id"));
        assert!(
            e.contains("not a commit id"),
            "`{bad}` should be refused as a commit id, got: {e}"
        );
    }
}

#[test]
fn a_full_commit_id_is_not_mistaken_for_a_path() {
    let cache = scratch("pin-ok");
    let rev = "0123456789abcdef0123456789abcdef01234567"; // 40 hex
    // It passes the commit-id check and fails later, offline, for want of a
    // cache -- never with the "not a commit id" message.
    let e = git::ensure(
        &cache,
        "https://example.com/x",
        &GitRef::Default,
        Some(rev),
        Net::Offline,
    )
    .expect_err("nothing is cached offline");
    assert!(
        !e.contains("not a commit id"),
        "a real commit id must not be refused as a path, got: {e}"
    );
}

// --- end to end: the injection never runs a command --------------------------

/// A package whose one git dependency has the given url.
fn app_with_dep_url(what: &str, url: &str) -> meadow_scratch::Scratch {
    let dir = scratch(what);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        format!(
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\nEvil = {{ git = \"{url}\", tag = \"v1.0.0\" }}\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/Main.mw"), "def result = 0\n").unwrap();
    dir
}

#[test]
fn building_a_hostile_git_url_runs_no_command() {
    // The payload would write this file if git ran it. It is an absolute path
    // in a scratch dir so that even a regression cannot touch the repo.
    let marks = scratch("mark");
    let mark = marks.join("PWNED");
    let url = format!("--upload-pack=touch {}", mark.display());
    let app = app_with_dep_url("inj", &url);

    let cache = scratch("inj-cache");
    let mut resolver = Resolver::for_entry(&app);
    resolver.cache = cache.to_path_buf();
    let out = pipeline::build_resolved(
        &app,
        meadow::Options::debug().entry("result"),
        &mut resolver,
    );

    assert!(
        !out.diagnostics.is_empty(),
        "a hostile git url should fail the build"
    );
    assert!(
        !mark.exists(),
        "the injected command must not have run: {} exists",
        mark.display()
    );
}

#[test]
fn resolving_a_hostile_url_directly_runs_no_command() {
    // `git::ensure` is the one path every fetch goes through.
    let marks = scratch("mark2");
    let mark = marks.join("PWNED2");
    let url = format!("--upload-pack=touch {}", mark.display());
    let cache = scratch("ens-cache");
    let e = git::ensure(&cache, &url, &GitRef::Default, None, Net::Allowed)
        .expect_err("a hostile url is refused");
    assert!(
        e.contains("option") || e.contains("refusing"),
        "expected a refusal, got: {e}"
    );
    assert!(!mark.exists(), "the injected command must not have run");
}
