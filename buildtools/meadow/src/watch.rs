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
//!
//! A build the program cannot take -- one that starts threads where the
//! program was not set up for them, say -- is refused, and the program runs
//! on as it was. The interpreter says so at [`REPORT`], and this offers to
//! start the program over: saving again does, with the build made then.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Set for the build `run --watch` makes on each change: the program is
/// running already, so the build stops once its image is written.
pub const REBUILD: &str = "MEADOW_WATCH_REBUILD";

/// Where the interpreter says how each reload ended: a line a reload,
/// `reloaded` or `refused`, a tab, and what it said of it.
pub const REPORT: &str = "MEADOW_WATCH_REPORT";

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
    let report = std::env::temp_dir().join(format!("meadow-watch-{}.report", std::process::id()));
    let _ = std::fs::remove_file(&report);
    program.env(REPORT, &report);
    let mut child = match program.spawn() {
        Ok(c) => c,
        Err(e) => {
            crate::status::error(format!("could not run the program: {e}"));
            return 1;
        }
    };
    let mut seen = sources(root);
    // How much of the report has been read, and whether a start over is on
    // offer: a reload was refused, and the program still runs the build
    // before it.
    let mut read = 0;
    let mut offered = false;
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(1),
            Ok(None) => {}
            Err(_) => break 1,
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        for (how, what) in reported(&report, &mut read) {
            match how.as_str() {
                "refused" => {
                    crate::status::warning(format!(
                        "the running program cannot take this build ({what}): \
                         save again to start it over with the new build"
                    ));
                    offered = true;
                }
                _ => offered = false,
            }
        }
        let now = sources(root);
        if now == seen {
            continue;
        }
        seen = now;
        if offered {
            // Taken up: the program over, from a build of the sources as
            // they are now.
            crate::status::status("Restarting", "with the new build");
            let _ = child.kill();
            let _ = child.wait();
            rebuild();
            let _ = std::fs::remove_file(&report);
            read = 0;
            offered = false;
            child = match program.spawn() {
                Ok(c) => c,
                Err(e) => {
                    crate::status::error(format!("could not run the program: {e}"));
                    break 1;
                }
            };
            continue;
        }
        crate::status::status("Rebuilding", "the sources changed");
        rebuild();
    };
    let _ = std::fs::remove_file(&report);
    ended
}

/// This command again, `--watch` and all, but only to build: the build
/// writes the image, which the interpreter watching it loads.
fn rebuild() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let args = std::env::args_os().skip(1);
    let _ = Command::new(exe).args(args).env(REBUILD, "1").status();
}

/// What the report at `path` says past the first `read` bytes, a pair a
/// reload: how it ended, and what was said of it.
fn reported(path: &Path, read: &mut usize) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Some(new) = text.get(*read..) else {
        return Vec::new();
    };
    // Only whole lines: the interpreter may be writing the last.
    let whole = new.rfind('\n').map_or(0, |i| i + 1);
    *read += whole;
    new[..whole]
        .lines()
        .map(|l| {
            let (how, what) = l.split_once('\t').unwrap_or((l, ""));
            (how.to_string(), what.to_string())
        })
        .collect()
}

/// What the package's sources are, as when each was last written: its
/// files, but for what builds write. A debugging session that watches
/// (`dap::silo`) looks at them as this does.
pub(crate) fn sources(root: &Path) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
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
