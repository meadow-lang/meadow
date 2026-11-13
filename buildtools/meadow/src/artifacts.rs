//! **Where a build puts what it makes.**
//!
//! Everything a build writes goes under the package's own `target` directory,
//! beside its `Meadow.toml`, one directory per profile:
//!
//! ```text
//!   <package>/target/debug/bytecode/<name>.mbc     the image the VM runs
//!   <package>/target/debug/bytecode/<name>.mbc.txt the image as text (`--emit bytecode`)
//!   <package>/target/debug/native/                 object code and binaries
//!   <package>/target/debug/native/<name>.s         native code as text (`--emit asm`)
//!   <package>/target/debug/incremental/            compiled packages, to reuse
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

/// Extension of a bytecode image -- see `meadow_bytecode::image`.
pub const IMAGE_EXTENSION: &str = "mbc";

/// `<root>/target/<profile>`.
pub fn profile_dir(root: &Path, profile: Profile) -> PathBuf {
    root.join(TARGET).join(profile.name())
}

/// Where the bytecode image of package `name` goes.
pub fn image_path(root: &Path, profile: Profile, name: &str) -> PathBuf {
    profile_dir(root, profile)
        .join("bytecode")
        .join(format!("{name}.{IMAGE_EXTENSION}"))
}

/// Where the image of package `name` goes as text -- `--emit bytecode`.
pub fn bytecode_text_path(root: &Path, profile: Profile, name: &str) -> PathBuf {
    profile_dir(root, profile)
        .join("bytecode")
        .join(format!("{name}.{IMAGE_EXTENSION}.txt"))
}

/// Where native object code and binaries go.
pub fn native_dir(root: &Path, profile: Profile) -> PathBuf {
    profile_dir(root, profile).join("native")
}

/// Where compiled packages are kept for the next build to reuse -- see
/// [`crate::incremental`]. By the profile's name, which the compiler options
/// carry.
pub fn incremental_dir(root: &Path, profile: &str) -> PathBuf {
    root.join(TARGET).join(profile).join("incremental")
}

/// Write `image` for package `name`, answering where it went.
pub fn write_image(
    root: &Path,
    profile: Profile,
    name: &str,
    image: &meadow_bytecode::Program,
) -> Result<PathBuf, String> {
    let path = image_path(root, profile, name);
    write(&path, &meadow_bytecode::image::encode(image))?;
    Ok(path)
}

/// Write `text` to `path`, making the directories on the way.
pub fn write_text(path: &Path, text: &str) -> Result<(), String> {
    write(path, text.as_bytes())
}

/// Write `bytes` to `path`, making the directories on the way.
pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    write(path, bytes)
}

/// Write `bytes` to `path`, making the directories on the way.
fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_is_under_the_package_target_directory() {
        let root = Path::new("/p");
        assert_eq!(
            image_path(root, Profile::Debug, "demo"),
            Path::new("/p/target/debug/bytecode/demo.mbc")
        );
        assert_eq!(
            native_dir(root, Profile::Release),
            Path::new("/p/target/release/native")
        );
        assert_eq!(
            bytecode_text_path(root, Profile::Debug, "demo"),
            Path::new("/p/target/debug/bytecode/demo.mbc.txt")
        );
    }
}
