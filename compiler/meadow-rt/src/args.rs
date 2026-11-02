//! **A program's command-line arguments**, as `Process.argv` answers them.
//!
//! Both machines answer `argv`, so what it means lives here rather than in
//! either. A native executable is the program, so its arguments are the
//! process's, less its own name. A program run by `meadow run` is not the
//! process -- `meadow` is -- and the driver says what the program was given
//! (what followed `--`) before running it.

use std::sync::OnceLock;

static GIVEN: OnceLock<Vec<String>> = OnceLock::new();

/// The arguments the program running in this process was given, when the
/// process is not the program itself. Only the first call counts.
pub fn set(args: Vec<String>) {
    let _ = GIVEN.set(args);
}

/// The program's arguments, not including its name.
pub fn get() -> Vec<String> {
    match GIVEN.get() {
        Some(args) => args.clone(),
        None => std::env::args().skip(1).collect(),
    }
}

/// When the file `md` describes was last written, in milliseconds since the
/// Unix epoch, as `Fs.metadata` answers it: 0 where the platform cannot say.
pub fn modified_millis(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// The path of the program running in this process, as `Process.currentExe`
/// answers it: what a build tool tells one compiler from another by. Empty
/// when the platform cannot say.
pub fn current_exe() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// The directory this process runs in, as `Process.currentDir` answers it.
/// Empty when the platform cannot say, as when the directory has been removed.
pub fn current_dir() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}
