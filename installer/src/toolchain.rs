//! What a release archive holds, and how it is laid out in `bin`.
//!
//! An archive is one of two things.
//!
//! * **The Rust `meadow` alone**, with `meadowup` beside it: every release
//!   before MeadowBoot shipped as `meadow`, and still the build for the
//!   targets MeadowBoot has none for (ARM Windows, Android). One
//!   self-contained file, the standard library built into it.
//! * **A MeadowBoot toolchain**: MeadowBoot, the compiler written in Meadow;
//!   `meadow-host`, the Rust back end as a library MeadowBoot loads into its
//!   own process; the Rust `meadow`, as the fallback front end; and `lib/`,
//!   the standard library's sources, MeadowBoot's precompiled `Std.mwstd`,
//!   and Silo's runtime for each phone. MeadowBoot finds all of it beside
//!   its own executable, so all of it is installed together.
//!
//! In the archive MeadowBoot is `MeadowBoot` and the Rust compiler is
//! `meadow`, so that a `meadowup` older than this module, which installs
//! only the file called `meadow`, is handed the Rust compiler -- one file
//! that works on its own -- rather than MeadowBoot without what it needs.
//! Installed, MeadowBoot becomes `meadow` and the Rust compiler
//! `meadow-rust`, which MeadowBoot looks for beside itself (and runs only
//! for what it hands on): `meadow` on the `PATH` is MeadowBoot's own
//! executable, not a shim in front of it.
//!
//! The toolchain goes in `bin` itself, the directory already on the `PATH`,
//! as `bin/lib/...` beside the executables. Not `~/.meadow/lib`, where the
//! Rust `meadow` keeps the standard library and runtime libraries it
//! extracts and compiles, which a toolchain replaced wholesale on every
//! update must not touch; and not a directory of its own, which would have
//! to join the `PATH` -- or be reached through a link, which macOS reports
//! as the program's own path, and MeadowBoot would look beside the link for
//! its library.

use std::path::{Path, PathBuf};

/// An executable's file name on this system.
fn exe(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// MeadowBoot's name in an archive.
pub fn boot_name() -> String {
    exe("MeadowBoot")
}

/// The Rust compiler's name in an archive, and MeadowBoot's once installed.
pub fn meadow_name() -> String {
    exe("meadow")
}

/// The Rust compiler's name installed beside MeadowBoot, as the fallback
/// front end it hands to.
pub fn rust_name() -> String {
    exe("meadow-rust")
}

/// `meadow-host`, the Rust back end as a library, as this system names one.
pub fn host_name() -> &'static str {
    if cfg!(windows) {
        "meadow_host.dll"
    } else if cfg!(target_os = "macos") {
        "libmeadow_host.dylib"
    } else {
        "libmeadow_host.so"
    }
}

/// The directory of the standard library and the phone runtimes, beside the
/// executables.
pub const LIB: &str = "lib";

/// What an unpacked archive holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Archive {
    /// The Rust `meadow` alone, at this path.
    Single(PathBuf),
    /// A MeadowBoot toolchain, whose files are in this directory.
    Toolchain(PathBuf),
}

impl Archive {
    /// What is unpacked under `dir`: a toolchain wherever MeadowBoot is, and
    /// otherwise the Rust `meadow` alone, if there is one.
    pub fn read(dir: &Path) -> Option<Archive> {
        if let Some(boot) = crate::release::find(dir, &boot_name()) {
            return boot.parent().map(|d| Archive::Toolchain(d.to_path_buf()));
        }
        crate::release::find(dir, &meadow_name()).map(Archive::Single)
    }
}

/// One component as it was put in place: its name and its size.
pub type Placed = (String, u64);

/// Put what `archive` holds in `bin`: MeadowBoot as `meadow` and everything
/// beside it, or the Rust `meadow` alone. Whatever a toolchain put there
/// before and this one does not have is taken away, so that an archive of
/// the Rust compiler alone leaves no MeadowBoot's library beside it. What
/// was installed is answered, in order, for saying.
pub fn install(archive: &Archive, bin: &Path) -> Result<Vec<Placed>, String> {
    std::fs::create_dir_all(bin).map_err(|e| format!("could not make {}: {e}", bin.display()))?;
    let mut placed = Vec::new();
    match archive {
        Archive::Single(meadow) => {
            placed.push((
                "meadow".to_string(),
                place(meadow, &bin.join(meadow_name()))?,
            ));
            // A toolchain's, from an install before this one.
            remove(&bin.join(rust_name()));
            remove(&bin.join(host_name()));
            remove_dir(&bin.join(LIB))?;
        }
        Archive::Toolchain(root) => {
            let boot = root.join(boot_name());
            let rust = root.join(meadow_name());
            let host = root.join(host_name());
            let lib = root.join(LIB);
            for (what, there) in [("MeadowBoot", &boot), ("meadow-host", &host)] {
                if !there.is_file() {
                    return Err(format!("{} holds no {what}", root.display()));
                }
            }
            if !lib.join("Std").is_dir() {
                return Err(format!(
                    "{} holds no standard library (lib/Std)",
                    root.display()
                ));
            }
            // The library first: a MeadowBoot put in place before it would
            // run, if it were run, with the last toolchain's library.
            placed.push(("lib".to_string(), replace_dir(&lib, &bin.join(LIB))?));
            placed.push((
                "meadow-host".to_string(),
                place(&host, &bin.join(host_name()))?,
            ));
            if rust.is_file() {
                placed.push((
                    "meadow-rust".to_string(),
                    place(&rust, &bin.join(rust_name()))?,
                ));
            } else {
                remove(&bin.join(rust_name()));
            }
            placed.push((
                "meadow".to_string(),
                place(&boot, &bin.join(meadow_name()))?,
            ));
        }
    }
    Ok(placed)
}

/// Copy `from` to `dest`, moving what was there aside first -- a running
/// program's image, or a library one has loaded, cannot be deleted on
/// Windows, but can be renamed -- and mark it runnable. Its size.
pub fn place(from: &Path, dest: &Path) -> Result<u64, String> {
    let backup = crate::displace(dest);
    std::fs::copy(from, dest).map_err(|e| format!("could not write {}: {e}", dest.display()))?;
    make_runnable(dest);
    // The displaced image may still be locked by a running process, in
    // which case leaving it is right and the next install clears it.
    if let Some(backup) = backup {
        let _ = std::fs::remove_file(backup);
    }
    Ok(std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0))
}

/// Take `file` away, or move it aside where it is in use.
fn remove(file: &Path) {
    if let Some(backup) = crate::displace(file) {
        let _ = std::fs::remove_file(backup);
    }
}

/// Where a directory being replaced is moved first.
fn aside(dir: &Path) -> PathBuf {
    crate::backup_path(dir)
}

/// Take the directory `dir` away: moved aside, then removed.
fn remove_dir(dir: &Path) -> Result<(), String> {
    let old = aside(dir);
    let _ = std::fs::remove_dir_all(&old);
    if dir.exists() {
        std::fs::rename(dir, &old).map_err(|e| {
            format!(
                "could not move {} aside ({e}): close any `meadow` that is running, and install again",
                dir.display()
            )
        })?;
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Make `dest` a copy of the directory `from`, with nothing of what was
/// there before left in it: a source file the last standard library had and
/// this one does not would otherwise be compiled as part of it. Its size.
fn replace_dir(from: &Path, dest: &Path) -> Result<u64, String> {
    remove_dir(dest)?;
    copy_dir(from, dest)
}

fn copy_dir(from: &Path, dest: &Path) -> Result<u64, String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("could not make {}: {e}", dest.display()))?;
    let entries =
        std::fs::read_dir(from).map_err(|e| format!("could not read {}: {e}", from.display()))?;
    let mut size = 0;
    for entry in entries {
        let entry = entry.map_err(|e| format!("could not read {}: {e}", from.display()))?;
        let path = entry.path();
        let to = dest.join(entry.file_name());
        if path.is_dir() {
            size += copy_dir(&path, &to)?;
        } else {
            size += std::fs::copy(&path, &to)
                .map_err(|e| format!("could not write {}: {e}", to.display()))?;
        }
    }
    Ok(size)
}

/// Mark a freshly written binary executable, which matters everywhere but
/// Windows.
#[cfg(unix)]
pub fn make_runnable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
pub fn make_runnable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch_dir;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// An unpacked toolchain archive, under a directory of the release's
    /// name as an archive may have it, whose files say which release they
    /// are from.
    fn toolchain(dir: &Path, release: &str, std_files: &[&str]) -> PathBuf {
        let root = dir.join(format!("meadow-{release}"));
        write(&root.join(boot_name()), &format!("boot {release}"));
        write(&root.join(meadow_name()), &format!("rust {release}"));
        write(&root.join(host_name()), &format!("host {release}"));
        write(&root.join("meadowup"), "meadowup");
        write(
            &root.join("lib/Std/Meadow.toml"),
            "[package]\nname = \"Std\"\n",
        );
        for f in std_files {
            write(&root.join("lib/Std/src").join(f), release);
        }
        write(&root.join("lib/Std.mwstd"), release);
        write(
            &root.join("lib/aarch64-linux-android/libmeadow_silo.a"),
            release,
        );
        dir.to_path_buf()
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn an_archive_of_the_rust_meadow_alone_is_one_file() {
        let dir = scratch_dir("meadowup-single").unwrap();
        write(&dir.join(meadow_name()), "rust");
        write(&dir.join("meadowup"), "meadowup");
        assert_eq!(
            Archive::read(&dir),
            Some(Archive::Single(dir.join(meadow_name())))
        );
    }

    #[test]
    fn an_archive_with_meadowboot_in_it_is_a_toolchain_wherever_it_is() {
        let dir = scratch_dir("meadowup-read").unwrap();
        toolchain(&dir, "a", &["Prelude.mw"]);
        assert_eq!(
            Archive::read(&dir),
            Some(Archive::Toolchain(dir.join("meadow-a")))
        );
        let empty = scratch_dir("meadowup-empty").unwrap();
        assert_eq!(Archive::read(&empty), None);
    }

    #[test]
    fn a_toolchain_is_installed_with_meadowboot_as_meadow() {
        let dir = scratch_dir("meadowup-tc").unwrap();
        let unpacked = toolchain(&dir.join("unpacked"), "a", &["Prelude.mw"]);
        let bin = dir.join("bin");
        let placed = install(&Archive::read(&unpacked).unwrap(), &bin).unwrap();
        let names: Vec<&str> = placed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["lib", "meadow-host", "meadow-rust", "meadow"]);
        assert_eq!(read(&bin.join(meadow_name())), "boot a");
        assert_eq!(read(&bin.join(rust_name())), "rust a");
        assert_eq!(read(&bin.join(host_name())), "host a");
        assert_eq!(read(&bin.join("lib/Std/src/Prelude.mw")), "a");
        assert_eq!(read(&bin.join("lib/Std.mwstd")), "a");
        assert_eq!(
            read(&bin.join("lib/aarch64-linux-android/libmeadow_silo.a")),
            "a"
        );
        assert!(!bin.join(boot_name()).exists(), "MeadowBoot kept its name");
        // The archive is the caller's, and is left as it was.
        assert!(unpacked.join("meadow-a").join(boot_name()).is_file());
    }

    #[test]
    fn an_update_leaves_nothing_of_the_last_standard_library() {
        let dir = scratch_dir("meadowup-update").unwrap();
        let bin = dir.join("bin");
        let a = toolchain(&dir.join("a"), "a", &["Prelude.mw", "Gone.mw"]);
        install(&Archive::read(&a).unwrap(), &bin).unwrap();
        let b = toolchain(&dir.join("b"), "b", &["Prelude.mw"]);
        install(&Archive::read(&b).unwrap(), &bin).unwrap();
        assert_eq!(read(&bin.join(meadow_name())), "boot b");
        assert_eq!(read(&bin.join("lib/Std/src/Prelude.mw")), "b");
        assert!(
            !bin.join("lib/Std/src/Gone.mw").exists(),
            "a source of the last library was left"
        );
        assert!(
            !bin.join("lib.old").exists(),
            "the last library was left aside"
        );
    }

    #[test]
    fn the_rust_meadow_alone_over_a_toolchain_takes_the_toolchain_away() {
        let dir = scratch_dir("meadowup-down").unwrap();
        let bin = dir.join("bin");
        let a = toolchain(&dir.join("a"), "a", &["Prelude.mw"]);
        install(&Archive::read(&a).unwrap(), &bin).unwrap();
        let single = dir.join("single");
        write(&single.join(meadow_name()), "rust old");
        install(&Archive::read(&single).unwrap(), &bin).unwrap();
        assert_eq!(read(&bin.join(meadow_name())), "rust old");
        for gone in [rust_name(), host_name().to_string(), LIB.to_string()] {
            assert!(!bin.join(&gone).exists(), "{gone} was left");
        }
    }

    #[test]
    fn a_toolchain_without_its_library_is_refused_before_anything_is_touched() {
        let dir = scratch_dir("meadowup-broken").unwrap();
        let bin = dir.join("bin");
        write(&bin.join(meadow_name()), "what was here");
        let broken = dir.join("broken");
        write(&broken.join(boot_name()), "boot");
        write(&broken.join(host_name()), "host");
        let err = install(&Archive::read(&broken).unwrap(), &bin).unwrap_err();
        assert!(err.contains("standard library"), "{err}");
        assert_eq!(read(&bin.join(meadow_name())), "what was here");
    }
}
