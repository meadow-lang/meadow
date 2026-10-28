//! **Where a build puts what it makes.**
//!
//! Everything a build writes goes under the package's own `target` directory,
//! beside its `Meadow.toml`, one directory per profile:
//!
//! ```text
//!   <package>/target/debug/native/        the image the VM runs, executables
//!   <package>/target/debug/incremental/   compiled packages, to reuse
//!   <package>/target/release/...
//! ```
//!
//! One place, so that cleaning is deleting it and ignoring it is one line --
//! which `meadow init` writes (see [`crate::init`]). Nothing in it is a source
//! of truth: every file can be made again from the package.

use crate::profile::Profile;
use std::path::{Path, PathBuf};

/// The directory's name, under a package root.
pub const TARGET: &str = "target";

/// `<root>/target/<profile>`.
pub fn profile_dir(root: &Path, profile: Profile) -> PathBuf {
    root.join(TARGET).join(profile.name())
}

/// Where images and native code go.
pub fn native_dir(root: &Path, profile: Profile) -> PathBuf {
    profile_dir(root, profile).join("native")
}

/// Where compiled packages are kept for the next build to reuse -- see
/// [`crate::incremental`]. By the profile's name, which the compiler options
/// carry.
pub fn incremental_dir(root: &Path, profile: &str) -> PathBuf {
    root.join(TARGET).join(profile).join("incremental")
}

/// Write `text` to `path`, making the directories on the way.
pub fn write_text(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_is_under_the_package_target_directory() {
        let root = Path::new("/p");
        assert_eq!(
            native_dir(root, Profile::Release),
            Path::new("/p/target/release/native")
        );
        assert_eq!(
            incremental_dir(root, "debug"),
            Path::new("/p/target/debug/incremental")
        );
    }
}
