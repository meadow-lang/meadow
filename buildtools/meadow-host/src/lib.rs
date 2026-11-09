//! The `meadow` command line as a C library.
//!
//! MeadowBoot, the compiler written in Meadow, is to be what runs when
//! `meadow` is typed. What it does not do itself yet -- the back end that
//! builds its Cut into a program, the package tools, the editor's tools -- is
//! Rust's, and it used to start the Rust `meadow` as a second process for
//! each. This library is the same command line loaded into MeadowBoot's own
//! process instead, called through `Std.Ffi`:
//!
//! ```text
//! int64_t meadow_main(const char *args);   // a JSON array of strings
//! const char *meadow_host_version(void);   // "0.5.0-dev", as `meadow --version`
//! ```
//!
//! `args` is the command line after the program's name, as MeadowBoot's
//! `Proc.argv ()` has it. The answer is the status `meadow` would have ended
//! with, unless the command ends the process itself, as one that fails does.
//! It is 64 bits wide because `Std.Ffi`'s `callInt` reads the whole register,
//! and a C `int` leaves the top half of it to chance.

use std::ffi::{CStr, CString, c_char};
use std::io::Write;

/// Runs `args`, a JSON array of strings, as `meadow` would run them.
///
/// # Safety
///
/// `args` is null or a NUL-terminated string that lasts the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_main(args: *const c_char) -> i64 {
    if args.is_null() {
        eprintln!("meadow-host: no command line was given");
        return 2;
    }
    // SAFETY: the caller hands a NUL-terminated string that lasts the call.
    let text = unsafe { CStr::from_ptr(args) }.to_string_lossy();
    let words: Vec<String> = match serde_json::from_str(&text) {
        Ok(words) => words,
        Err(e) => {
            eprintln!("meadow-host: the command line is not a JSON array of strings: {e}");
            return 2;
        }
    };
    let status = meadow::cli::main(std::iter::once("meadow".to_string()).chain(words));
    // A library's standard output is its own buffer, which nothing flushes
    // when the program that loaded it exits.
    let _ = std::io::stdout().flush();
    status.into()
}

/// What `meadow --version` says after the name, for MeadowBoot to say which
/// library it loaded. The string lives as long as the library.
#[unsafe(no_mangle)]
pub extern "C" fn meadow_host_version() -> *const c_char {
    static VERSION: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| CString::new(meadow::channel::VERSION).expect("no NUL in a version"))
        .as_ptr()
}
