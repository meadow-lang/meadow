//! `meadow run --watch`: run a program on Silo's interpreter and, whenever
//! the package's sources change, build it again and load the new build into
//! the program as it runs (`docs/RUNTIME-MERGE.md`, "Hot loading").
//!
//! The interpreter does the loading: started with `--watch`, it reads its
//! image again whenever the image is written anew, and from then on a call by
//! name runs the new version -- what is running already finishes as it was.
//! This side only builds: each change runs this same command again, with
//! [`REBUILD`] set, which builds as the first did, writes the image, and
//! stops short of running it.
//! A build that fails says why and leaves the program running as it was.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Set for the build `run --watch` makes on each change: the program is
/// running already, so the build stops once its image is written.
pub const REBUILD: &str = "MEADOW_WATCH_REBUILD";

static WATCHING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Run with `--watch`.
pub fn set_watching() {
    let _ = WATCHING.set(true);
}

pub fn watching() -> bool {
    WATCHING.get().copied().unwrap_or(false)
}

/// Whether this is one of the builds a watching run makes.
pub fn rebuilding() -> bool {
    std::env::var_os(REBUILD).is_some()
}

/// Run `program` -- the interpreter, watching its image -- and rebuild the
/// package at `root` whenever its sources change, until the program ends:
/// answers how it ended.
pub fn run(mut program: Command, root: &Path) -> i32 {
    let mut child = match program.spawn() {
        Ok(c) => c,
        Err(e) => {
            crate::status::error(format!("could not run the program: {e}"));
            return 1;
        }
    };
    let mut seen = sources(root);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(1),
            Ok(None) => {}
            Err(_) => return 1,
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        let now = sources(root);
        if now == seen {
            continue;
        }
        seen = now;
        crate::status::status("Rebuilding", "the sources changed");
        // This command again, `--watch` and all, but only to build.
        let Ok(exe) = std::env::current_exe() else {
            continue;
        };
        let args = std::env::args_os().skip(1);
        let _ = Command::new(exe).args(args).env(REBUILD, "1").status();
    }
}

/// What the package's sources are, as when each was last written: its
/// files, but for what builds write.
fn sources(root: &Path) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let Ok(meta) = e.metadata() else {
                continue;
            };
            if meta.is_dir() {
                dirs.push(path);
            } else {
                out.push((path, meta.len(), meta.modified().ok()));
            }
        }
    }
    out.sort();
    out
}
