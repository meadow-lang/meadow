//! Which channel this `meadow` was built for, and what that lets a package
//! ask of it.
//!
//! The split is Rust's. A **stable** build is a release with a version that
//! does not move, and it accepts only what the language promises to keep. A
//! **nightly** build is `master` on some day, and accepts features that are
//! still unsettled, each named at the top of the root module of the package
//! that uses it, as `#![feature(…)]` is at the top of a crate's:
//!
//! ```text
//! @!feature(name)
//! ```
//!
//! There are none just now: [`UNSTABLE`] is empty. `ffi`, calling C through
//! `Std.Ffi`, was the first, and is settled: see [`STABILISED`].
//!
//! A build from a checkout is **dev**, and takes what nightly takes. These
//! are not the features a package offers the ones that build it, which are
//! `[features]` in its `Meadow.toml` and work on every channel.
//!
//! The channel is decided when `meadow` is built -- `MEADOW_CHANNEL`, which
//! the release workflows set; see `build.rs` -- so a binary cannot be talked
//! into another one.

use std::path::Path;

/// `stable`, `nightly` or `dev`.
pub const CHANNEL: &str = env!("MEADOW_CHANNEL");

/// What `meadow --version` says after the name: `0.2.0`, `0.3.0-nightly
/// (2026-10-05 892808f)`, `0.3.0-dev`.
pub const VERSION: &str = env!("MEADOW_VERSION");

/// The version alone, as the manifests have it.
pub const NUMBER: &str = env!("CARGO_PKG_VERSION");

/// The features that are not settled: each one's name in `@!feature(…)`,
/// and what it is, for the error that asks for it.
pub const UNSTABLE: &[(&str, &str)] = &[];

/// The features that were unstable once and are settled now: each one's name
/// and the release that settled it. A package that still asks for one builds
/// on every channel, as Rust takes a `#![feature(…)]` that has since been
/// stabilised, and is told that the line can go.
pub const STABILISED: &[(&str, &str)] = &[("ffi", "0.5.0")];

/// The file beside a `Meadow.toml` that says which toolchain the project is
/// built with: a channel, or a version.
pub const TOOLCHAIN_FILE: &str = "meadow-toolchain";

/// What is wrong with a package asking for `feature` on `channel`, if
/// anything. A feature that is settled is not wrong to ask for, on any
/// channel: [`feature_settled`] says what such a package is told instead.
pub fn feature_problem(feature: &str, channel: &str) -> Option<String> {
    problem_among(UNSTABLE, feature, channel)
}

/// [`feature_problem`], with `unstable` for the features that are not
/// settled: so that what is said of one can be tested while there is none.
fn problem_among(unstable: &[(&str, &str)], feature: &str, channel: &str) -> Option<String> {
    if feature_settled(feature).is_some() {
        return None;
    }
    if !unstable.iter().any(|(name, _)| *name == feature) {
        let known: Vec<String> = unstable.iter().map(|(n, _)| format!("`{n}`")).collect();
        let has = if known.is_empty() {
            "it has no unstable features".to_string()
        } else {
            format!("the ones it has are {}", known.join(", "))
        };
        return Some(format!(
            "`{feature}` is not a feature this `meadow` knows: {has}"
        ));
    }
    (channel == "stable").then(|| {
        format!(
            "the feature `{feature}` is unstable, and this is a stable `meadow`: \
             a nightly one accepts it (`meadowup default nightly`, then `meadowup update`)"
        )
    })
}

/// The warning for a package that asks for `feature` when it is settled:
/// that it needs asking for no longer. `None` for any other feature.
pub fn feature_settled(feature: &str) -> Option<String> {
    STABILISED
        .iter()
        .find(|(name, _)| *name == feature)
        .map(|(name, since)| {
            format!(
                "the feature `{name}` is stable since {since}, and is not asked for any more: \
                 `@!feature({name})` can go"
            )
        })
}

/// What is wrong with building here, when the project's toolchain file asks
/// for something this `meadow` is not: `wanted` is the file's one word.
pub fn toolchain_problem(wanted: &str, channel: &str, number: &str) -> Option<String> {
    let wanted = wanted.trim();
    let bare = wanted.trim_start_matches('v');
    let fits = match wanted {
        "" => true,
        // A build from a checkout stands in for either channel: it is what
        // the person working on Meadow has.
        "stable" | "nightly" => channel == wanted || channel == "dev",
        _ => bare == number,
    };
    (!fits).then(|| {
        let have = match channel {
            "stable" => number.to_string(),
            other => format!("{number} ({other})"),
        };
        let get = match wanted {
            "stable" | "nightly" => {
                format!("`meadowup default {wanted}`, then `meadowup update`")
            }
            _ => format!("`meadowup install {bare}`"),
        };
        format!(
            "this project is built with meadow {wanted}, as its `{TOOLCHAIN_FILE}` says, \
             and this is meadow {have}: {get}"
        )
    })
}

/// [`toolchain_problem`] for the project at `root`, read from its toolchain
/// file when it has one. A comment line starts with `#`.
pub fn check_toolchain(root: &Path) -> Option<String> {
    // The nearest one at or above the project, as `rust-toolchain` is found:
    // a workspace's covers its members.
    let root = root.canonicalize().ok()?;
    let start = if root.is_dir() {
        root.as_path()
    } else {
        root.parent()?
    };
    let text = start
        .ancestors()
        .find_map(|dir| std::fs::read_to_string(dir.join(TOOLCHAIN_FILE)).ok())?;
    let wanted = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    toolchain_problem(wanted, CHANNEL, NUMBER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feature_nobody_knows_is_said_so_on_every_channel() {
        for channel in ["stable", "nightly", "dev"] {
            let said = feature_problem("warp", channel).expect("a problem");
            assert!(said.contains("`warp` is not a feature"), "{said}");
            assert!(said.contains("it has no unstable features"), "{said}");

            let warp = [("warp", "going faster")];
            let said = problem_among(&warp, "weft", channel).expect("a problem");
            assert!(said.contains("the ones it has are `warp`"), "{said}");
        }
    }

    #[test]
    fn an_unstable_feature_is_refused_on_stable_and_nowhere_else() {
        let warp = [("warp", "going faster")];
        let said = problem_among(&warp, "warp", "stable").expect("a problem");
        assert!(said.contains("meadowup default nightly"), "{said}");
        assert_eq!(problem_among(&warp, "warp", "nightly"), None);
        assert_eq!(problem_among(&warp, "warp", "dev"), None);
    }

    #[test]
    fn a_settled_feature_is_taken_on_every_channel_and_said_to_be_settled() {
        for channel in ["stable", "nightly", "dev"] {
            assert_eq!(feature_problem("ffi", channel), None);
        }
        let said = feature_settled("ffi").expect("a warning");
        assert!(said.contains("`@!feature(ffi)` can go"), "{said}");
        assert_eq!(feature_settled("warp"), None);
    }

    #[test]
    fn a_toolchain_file_names_a_channel_or_a_version() {
        assert_eq!(toolchain_problem("stable", "stable", "0.2.0"), None);
        assert_eq!(toolchain_problem("nightly", "nightly", "0.3.0"), None);
        assert_eq!(toolchain_problem("0.2.0", "stable", "0.2.0"), None);
        assert_eq!(toolchain_problem("v0.2.0", "stable", "0.2.0"), None);
        assert_eq!(toolchain_problem("", "stable", "0.2.0"), None);

        let said = toolchain_problem("nightly", "stable", "0.2.0").expect("a problem");
        assert!(said.contains("meadowup default nightly"), "{said}");
        let said = toolchain_problem("0.3.0", "stable", "0.2.0").expect("a problem");
        assert!(said.contains("meadowup install 0.3.0"), "{said}");
    }

    #[test]
    fn a_build_from_a_checkout_stands_in_for_a_channel_but_not_a_version() {
        assert_eq!(toolchain_problem("stable", "dev", "0.3.0"), None);
        assert_eq!(toolchain_problem("nightly", "dev", "0.3.0"), None);
        assert!(toolchain_problem("0.2.0", "dev", "0.3.0").is_some());
    }
}
