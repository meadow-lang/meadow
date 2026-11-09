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
//! @!feature(ffi)
//!
//! use Std.Ffi as Ffi
//! ```
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
pub const UNSTABLE: &[(&str, &str)] = &[("ffi", "calling C through `Std.Ffi`")];

/// The file beside a `Meadow.toml` that says which toolchain the project is
/// built with: a channel, or a version.
pub const TOOLCHAIN_FILE: &str = "meadow-toolchain";

/// What is wrong with a package asking for `feature` on `channel`, if
/// anything.
pub fn feature_problem(feature: &str, channel: &str) -> Option<String> {
    if !UNSTABLE.iter().any(|(name, _)| *name == feature) {
        let known: Vec<String> = UNSTABLE.iter().map(|(n, _)| format!("`{n}`")).collect();
        return Some(format!(
            "`{feature}` is not a feature this `meadow` knows: the ones it has are {}",
            known.join(", ")
        ));
    }
    (channel == "stable").then(|| {
        format!(
            "the feature `{feature}` is unstable, and this is a stable `meadow`: \
             a nightly one accepts it (`meadowup default nightly`, then `meadowup update`)"
        )
    })
}

/// The channel whose features a package may ask for: this build's, except
/// that `MEADOW_BOOTSTRAP=1` lets a stable one take what nightly takes.
///
/// That is for building Meadow's own compiler, as `RUSTC_BOOTSTRAP` is for
/// rustc's: MeadowBoot calls the Rust back end through `Std.Ffi`, and a
/// release's stable `meadow` is what builds and tests it. Anything else that
/// sets it gives up what stable promises, by its own say.
pub fn accepting() -> &'static str {
    accepting_on(CHANNEL, std::env::var("MEADOW_BOOTSTRAP").ok().as_deref())
}

fn accepting_on(channel: &'static str, bootstrap: Option<&str>) -> &'static str {
    if channel == "stable" && bootstrap == Some("1") {
        "nightly"
    } else {
        channel
    }
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

/// What is wrong with a package that reaches `Std.Ffi` without having asked
/// for the feature: `asked` is what its root module's `@!feature(…)`s name.
pub fn ffi_problem(asked: &[String]) -> Option<String> {
    (!asked.iter().any(|f| f == "ffi")).then(|| {
        "`Std.Ffi` is unstable: a package that uses it says so, with `@!feature(ffi)` \
         at the top of its root module"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feature_nobody_knows_is_said_so_on_every_channel() {
        for channel in ["stable", "nightly", "dev"] {
            let said = feature_problem("warp", channel).expect("a problem");
            assert!(said.contains("`warp` is not a feature"), "{said}");
            assert!(said.contains("`ffi`"), "{said}");
        }
    }

    #[test]
    fn an_unstable_feature_is_refused_on_stable_and_nowhere_else() {
        let said = feature_problem("ffi", "stable").expect("a problem");
        assert!(said.contains("meadowup default nightly"), "{said}");
        assert_eq!(feature_problem("ffi", "nightly"), None);
        assert_eq!(feature_problem("ffi", "dev"), None);
    }

    #[test]
    fn the_compiler_s_own_build_takes_nightly_s_features_on_stable() {
        assert_eq!(accepting_on("stable", Some("1")), "nightly");
        assert_eq!(accepting_on("stable", None), "stable");
        assert_eq!(accepting_on("stable", Some("yes")), "stable");
        assert_eq!(accepting_on("dev", Some("1")), "dev");
    }

    #[test]
    fn calling_c_is_asked_for_by_name() {
        let with = vec!["ffi".to_string()];
        assert_eq!(ffi_problem(&with), None);
        let said = ffi_problem(&[]).expect("a problem");
        assert!(said.contains("@!feature(ffi)"), "{said}");
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
