//! `meadow clean`: remove what a build wrote.
//!
//! A `target` directory holds images, executables, native code and the
//! incremental cache -- everything that can be made again from the sources
//! beside it. Nothing else is touched: not the manifest, not `Meadow.lock`,
//! and not the dependency cache, which other packages share and which this has
//! no business emptying.
//!
//! In a workspace the members build into the root's `target`, so cleaning the
//! root is cleaning all of them; cleaning a member finds the root and says so
//! rather than looking for a directory that is not there.

use std::path::{Path, PathBuf};

/// What a clean found, for the caller to report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cleaned {
    /// The directories removed, or that would be.
    pub removed: Vec<PathBuf>,
    /// How many bytes they held.
    pub bytes: u64,
}

/// Remove `path`'s build output, or with `profile` only that profile's.
///
/// `dry_run` finds and measures without removing, which is what `--dry-run`
/// reports.
pub fn run(path: &Path, profile: Option<&str>, dry_run: bool) -> Result<Cleaned, String> {
    let root = crate::workspace::Workspace::load(path)
        .ok()
        .map(|ws| ws.root.clone())
        .or_else(|| crate::package::enclosing_root(&path.join("x")))
        .unwrap_or_else(|| path.to_path_buf());
    let target = root.join("target");
    if !target.is_dir() {
        return Ok(Cleaned::default());
    }

    let mut out = Cleaned::default();
    let remove = |dir: PathBuf, out: &mut Cleaned| -> Result<(), String> {
        if !dir.is_dir() {
            return Ok(());
        }
        out.bytes += size_of(&dir);
        out.removed.push(dir.clone());
        if !dry_run {
            std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Ok(())
    };

    match profile {
        Some(name) => remove(target.join(name), &mut out)?,
        None => remove(target, &mut out)?,
    }
    Ok(out)
}

/// Every byte under `dir`. Best effort: a file that cannot be read is a file
/// whose size is not counted, which is better than refusing to clean.
pub(crate) fn size_of(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for e in entries.flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => total += size_of(&e.path()),
            Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => {}
        }
    }
    total
}

/// Bytes, as a person reads them.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(who: &str) -> meadow_scratch::Scratch {
        meadow_scratch::scratch!("meadow-clean-{}-{who}", std::process::id())
    }

    /// A package with a `target` holding something in two profiles.
    fn package(dir: &Path) -> PathBuf {
        let root = dir.join("Pkg");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Meadow.toml"), "[package]\nname = \"Pkg\"\n").unwrap();
        std::fs::write(root.join("src/Main.mw"), "fun main () = ()\n").unwrap();
        for p in ["debug", "release"] {
            std::fs::create_dir_all(root.join("target").join(p)).unwrap();
            std::fs::write(root.join("target").join(p).join("image.mbc"), [0u8; 512]).unwrap();
        }
        root
    }

    #[test]
    fn cleaning_removes_the_target_directory_and_nothing_else() {
        let dir = scratch("all");
        let root = package(&dir);
        let out = run(&root, None, false).expect("clean");
        assert_eq!(out.removed, [root.join("target")]);
        assert!(!root.join("target").exists());
        assert!(root.join("Meadow.toml").is_file(), "the manifest stays");
        assert!(root.join("src/Main.mw").is_file(), "the sources stay");
    }

    #[test]
    fn one_profile_can_be_cleaned_on_its_own() {
        let dir = scratch("one");
        let root = package(&dir);
        run(&root, Some("debug"), false).expect("clean");
        assert!(!root.join("target/debug").exists());
        assert!(root.join("target/release").exists(), "the other stays");
    }

    /// `--dry-run` measures and reports without removing, which is the only
    /// way to be sure of what a clean would take with it.
    #[test]
    fn a_dry_run_removes_nothing() {
        let dir = scratch("dry");
        let root = package(&dir);
        let out = run(&root, None, true).expect("clean");
        assert_eq!(out.removed, [root.join("target")]);
        assert!(out.bytes >= 1024, "two files of 512 bytes: {}", out.bytes);
        assert!(root.join("target").is_dir(), "still there");
    }

    /// Nothing built yet is not an error; there is simply nothing to do.
    #[test]
    fn a_package_that_was_never_built_cleans_to_nothing() {
        let dir = scratch("never");
        let root = dir.join("Fresh");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Meadow.toml"), "[package]\nname = \"Fresh\"\n").unwrap();
        let out = run(&root, None, false).expect("clean");
        assert_eq!(out, Cleaned::default());
    }

    #[test]
    fn bytes_are_reported_the_way_people_read_them() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2.0 KiB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
