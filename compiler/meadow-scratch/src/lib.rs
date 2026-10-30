//! Scratch directories for tests, removed when the test is over -- passed or
//! failed.
//!
//! The suites used to write their directories into the system's temporary
//! directory and leave them there: most never removed theirs, and the ones that
//! did removed it on their last line, which a failing assertion never reaches.
//! Every run added thousands of directories, some of them a native build of a
//! few hundred megabytes, and they piled up into tens of gigabytes on the
//! system drive. A [`Scratch`] is removed when it is dropped, and a panic
//! unwinds through it like a return does, so a failing test cleans up as well.
//!
//! To look at a directory after a failure, set `MEADOW_KEEP_TEST_DIRS=1`:
//! nothing is removed then, and a failing test says what it left and where.
//!
//! [`scratch!`] puts the directory in the target directory's `tmp/`
//! (`CARGO_TARGET_TMPDIR`, which cargo gives integration tests and
//! benchmarks), so a run that is killed outright -- the one case a destructor
//! cannot help -- leaves its remains on the build's own drive, where
//! `cargo clean` takes them. Unit tests are not given that directory and use
//! the system's temporary one.

use std::ffi::OsStr;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Set to anything but `0` or nothing, and scratch directories are kept.
pub const KEEP: &str = "MEADOW_KEEP_TEST_DIRS";

/// A fresh, empty scratch directory named by the `format!` arguments, under
/// [`root`]: `scratch!("meadow-dep-{}-{what}", std::process::id())`.
#[macro_export]
macro_rules! scratch {
    ($($name:tt)+) => {
        $crate::Scratch::new(
            $crate::root(::core::option_env!("CARGO_TARGET_TMPDIR")),
            &::std::format!($($name)+),
        )
    };
}

/// Where scratch directories go: the target directory's `tmp/` when cargo
/// names one, else the system's temporary directory.
pub fn root(target_tmp: Option<&str>) -> PathBuf {
    match target_tmp {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    }
}

/// Whether `MEADOW_KEEP_TEST_DIRS` asks for scratch directories to be kept.
pub fn keeping() -> bool {
    std::env::var_os(KEEP).is_some_and(|v| !v.is_empty() && v != "0")
}

/// A directory that is removed when this is dropped.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
    kept: bool,
    /// Others that go with this one: a package's dependencies, made apart.
    held: Vec<Scratch>,
}

impl Scratch {
    /// The directory `name` under `root`, empty: whatever an earlier run left
    /// there is cleared first, so a fixed name is safe to reuse.
    #[track_caller]
    pub fn new(root: impl AsRef<Path>, name: &str) -> Scratch {
        let path = root.as_ref().join(name);
        remove(&path);
        if let Err(e) = std::fs::create_dir_all(&path) {
            panic!(
                "could not make the scratch directory {}: {e}",
                path.display()
            );
        }
        Scratch {
            path,
            kept: false,
            held: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This directory, removing `other` along with it: for a helper that
    /// makes a package and the packages it depends on, and answers only the
    /// first.
    pub fn holding(mut self, other: Scratch) -> Scratch {
        self.held.push(other);
        self
    }

    /// The directory `name` in this one, made, and removing this one with it.
    #[track_caller]
    pub fn child(self, name: &str) -> Scratch {
        let path = self.path.join(name);
        if let Err(e) = std::fs::create_dir_all(&path) {
            panic!(
                "could not make the scratch directory {}: {e}",
                path.display()
            );
        }
        Scratch {
            path,
            kept: false,
            held: vec![self],
        }
    }

    /// The same directory, named as `std::fs::canonicalize` names it.
    pub fn canonical(mut self) -> Scratch {
        if let Ok(full) = std::fs::canonicalize(&self.path) {
            self.path = full;
        }
        self
    }

    /// Leave the directory where it is when this is dropped, answering it.
    pub fn keep(mut self) -> PathBuf {
        self.kept = true;
        for other in std::mem::take(&mut self.held) {
            other.keep();
        }
        std::mem::take(&mut self.path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        let failing = std::thread::panicking();
        if keeping() {
            if failing {
                eprintln!("kept {} ({KEEP} is set)", self.path.display());
            }
            return;
        }
        remove(&self.path);
        if failing {
            eprintln!("removed {}; set {KEEP}=1 to keep it", self.path.display());
        }
    }
}

/// Remove `path` and everything in it, if it is there. A program that has
/// just exited can hold its own image open a moment longer on Windows, and a
/// virus scanner opens new files as they appear, so a refusal is tried again
/// a few times before it is given up on.
pub fn remove(path: &Path) {
    for attempt in 0..10u64 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            // Something that is not a directory where one was asked for.
            Err(_) if path.is_file() => {
                let _ = std::fs::remove_file(path);
                return;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20 * (attempt + 1))),
        }
    }
}

impl Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for Scratch {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}
