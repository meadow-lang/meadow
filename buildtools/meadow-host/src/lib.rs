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
//! const char *meadow_repl_run(const char *request);    // an entry of a prompt, run
//! const char *meadow_repl_pages(const char *request);  // an entry, in each IR
//! const char *meadow_debug(const char *request);       // a debugging session
//! ```
//!
//! `args` is the command line after the program's name, as MeadowBoot's
//! `Proc.argv ()` has it. The answer is the status `meadow` would have ended
//! with, unless the command ends the process itself, as one that fails does.
//! It is 64 bits wide because `Std.Ffi`'s `callInt` reads the whole register,
//! and a C `int` leaves the top half of it to chance.

use std::cell::RefCell;
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

/// An entry of MeadowBoot's prompt, run as `meadow repl` runs one of its own
/// (`meadow::cut::answered`). The request is JSON:
///
/// ```text
/// { "program": "<Cut>", "engine": "jit" | "vm", "opt": 0 | 1 | 2 }
/// ```
///
/// `program` is the entry's program of Cut, linked by the prompt: what it
/// reaches of the standard library and of the entries before it, and an
/// entry that answers its value as the text the prompt shows. The answer is
/// JSON too, `{ "value": "<text>" }` or `{ "error": "<why>" }`, with
/// `"compile"` and `"run"`, how long lowering and running it took in
/// nanoseconds. What the program prints is written as it runs, and standard
/// output is flushed before the answer is handed back.
///
/// The prompt keeps what its entries are made of, as `meadow repl` does, and
/// each entry is a program of its own: so nothing is kept here between two
/// calls. The answer lasts until the next call of either `meadow_repl_…` on
/// the same thread.
///
/// # Safety
///
/// `request` is null or a NUL-terminated string that lasts the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_repl_run(request: *const c_char) -> *const c_char {
    // SAFETY: as the caller promises.
    let answer = match unsafe { request_of(request) } {
        Err(why) => serde_json::json!({ "error": why }),
        Ok(r) => {
            let program = r["program"].as_str().unwrap_or_default();
            let engine = match r["engine"].as_str() {
                Some("vm") => meadow::runtime::Engine::Vm,
                _ => meadow::runtime::Engine::Jit,
            };
            let (result, times) = meadow::cut::answered(program, engine, opt_of(&r));
            let _ = std::io::stdout().flush();
            let mut answer = match result {
                Ok(value) => serde_json::json!({ "value": value }),
                Err(why) => serde_json::json!({ "error": why }),
            };
            answer["compile"] = times.compile.map_or(0, |d| d.as_nanos() as u64).into();
            answer["run"] = (times.run.as_nanos() as u64).into();
            answer
        }
    };
    kept(answer.to_string())
}

/// An entry of MeadowBoot's prompt in each IR the back end makes of it, for
/// its `:ir` (`meadow::ir::cut_pages`). The request is JSON:
///
/// ```text
/// { "program": "<Cut>", "own": ["<symbol>", ...], "only": "<form>" | null, "opt": 1 }
/// ```
///
/// `own` are the symbols of the definitions shown and of the local
/// functions made of them. The answer is the pages, each its title and its
/// text: `[["Cut", "..."], ["AxCut", "..."], ...]`. It lasts as
/// `meadow_repl_run`'s does.
///
/// # Safety
///
/// `request` is null or a NUL-terminated string that lasts the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_repl_pages(request: *const c_char) -> *const c_char {
    // SAFETY: as the caller promises.
    let answer = match unsafe { request_of(request) } {
        Err(why) => serde_json::json!([["error", why]]),
        Ok(r) => {
            let program = r["program"].as_str().unwrap_or_default();
            let own: Vec<String> = r["own"]
                .as_array()
                .map(|xs| {
                    xs.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let pages = meadow::ir::cut_pages(program, &own, opt_of(&r), r["only"].as_str());
            serde_json::Value::Array(
                pages
                    .into_iter()
                    .map(|p| serde_json::json!([p.title, p.text]))
                    .collect(),
            )
        }
    };
    kept(answer.to_string())
}

/// One request of a debugging session MeadowBoot's debug adapter holds open
/// (`meadow::dap::hosted`): a program of Cut built for debugging, launched
/// on Glade, then breakpoints set, run and stepped, and its stack, scopes
/// and values asked for. The request and the answer are JSON, as that
/// module says: `{ "do": "launch", "program": "<path>" }`, `{ "do": "run",
/// "budget": 20000 }`, and so on, each answered `{ "error": "<why>" }` where
/// it cannot be. The session lasts from one call to the next, until another
/// launch or `{ "do": "end" }`. The answer lasts as `meadow_repl_run`'s does.
///
/// # Safety
///
/// `request` is null or a NUL-terminated string that lasts the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_debug(request: *const c_char) -> *const c_char {
    // SAFETY: as the caller promises.
    let answer = match unsafe { request_of(request) } {
        Err(why) => serde_json::json!({ "error": why }),
        Ok(r) => meadow::dap::hosted::request(&r),
    };
    let _ = std::io::stdout().flush();
    kept(answer.to_string())
}

/// A request, read.
///
/// # Safety
///
/// `request` is null or a NUL-terminated string that lasts the call.
unsafe fn request_of(request: *const c_char) -> Result<serde_json::Value, String> {
    if request.is_null() {
        return Err("meadow-host: no request was given".into());
    }
    // SAFETY: the caller hands a NUL-terminated string that lasts the call.
    let text = unsafe { CStr::from_ptr(request) }.to_string_lossy();
    serde_json::from_str(&text).map_err(|e| format!("meadow-host: the request is not JSON: {e}"))
}

/// The optimisation level a request asks for: `meadow repl`'s, the debug
/// profile's, where it does not say.
fn opt_of(r: &serde_json::Value) -> meadow_compiler::OptLevel {
    match r["opt"].as_u64() {
        Some(0) => meadow_compiler::OptLevel::O0,
        Some(2) => meadow_compiler::OptLevel::O2,
        _ => meadow_compiler::OptLevel::O1,
    }
}

/// `answer` as a C string that lasts until the next answer on this thread.
fn kept(answer: String) -> *const c_char {
    thread_local! {
        static LAST: RefCell<CString> = RefCell::new(CString::default());
    }
    let answer = CString::new(answer).unwrap_or_default();
    LAST.with(|last| {
        *last.borrow_mut() = answer;
        last.borrow().as_ptr()
    })
}
