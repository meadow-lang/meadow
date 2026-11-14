//! A debugging session another program holds open: the back end's half of a
//! debugger whose protocol is spoken elsewhere.
//!
//! MeadowBoot's debug adapter (`bootstrap/src/Dap.mw`) speaks the Debug
//! Adapter Protocol itself, through Lingua's `dap!`, and what it debugs is a
//! program it built, as Cut. Running that program a step at a time on Glade
//! and saying where it is and what it holds is [`Session`]'s, as it is for
//! `meadow dap`; this is the same session, driven by requests that
//! `meadow-host` hands over as JSON (`meadow_debug`), one at a time:
//!
//! ```text
//! { "do": "launch", "program": "<a file of Cut>" }
//!     -> {} | { "stop": <stop> } | { "error": "<why>" }
//! { "do": "breakpoints", "path": "<file>", "lines": [3, 7] }
//!     -> { "lines": [3, null] }                the line each landed on
//! { "do": "resume", "mode": "continue" | "stepIn" | "stepOver" | "stepOut" } -> {}
//! { "do": "run", "budget": 20000 }  -> { "stop": <stop> | null, "output": "<text>" }
//! { "do": "frames", "max": 100 }    -> { "frames": [{ "name", "path", "line", "column" }] }
//! { "do": "scopes", "frame": 0 }    -> { "scopes": [{ "name", "reference", "expensive" }] }
//! { "do": "variables", "reference": 1 }
//!     -> { "variables": [{ "name", "value", "type", "reference" }] }
//! { "do": "evaluate", "frame": 0, "expression": "x" } -> { "variable": <row> | null }
//! { "do": "output" }                -> { "output": "<text>" }
//! { "do": "end" }                   -> {}
//! ```
//!
//! A stop is `{ "reason": "breakpoint" | "step" | "exception" | "exited" |
//! "failed", "text": "<what it failed with, or came to>" }`: `exception` when
//! it fails, which can be looked at but not run on, and `failed` asked after. A launch stands
//! the program at its first boundary -- where a launch that stops on entry
//! stops -- having stopped nowhere yet, so a breakpoint there still counts
//! when it is run on; it answers a stop only where the program ended before
//! reaching one. Lines and columns count from 1, and a path is as the
//! program's `source`s spell it.
//!
//! One session at a time, per process: a launch ends the one before it.

use super::session::{Mode, Session, Stop, Variable};
use serde_json::{Value as Json, json};
use std::path::Path;
use std::sync::Mutex;

/// The session, while one is open.
struct Held(Session);

// SAFETY: a session is only ever reached through `SESSION`'s lock, one
// request at a time, and nothing in it belongs to the thread that made it:
// its machine interprets bytecode, with no native code and no thread's own
// state, and what it writes goes to a buffer behind a lock of its own.
unsafe impl Send for Held {}

static SESSION: Mutex<Option<Held>> = Mutex::new(None);

/// The answer to one request.
pub fn request(req: &Json) -> Json {
    let mut held = SESSION.lock().unwrap_or_else(|p| p.into_inner());
    let what = req["do"].as_str().unwrap_or_default();
    if what == "launch" {
        *held = None;
        let Some(program) = req["program"].as_str() else {
            return json!({ "error": "`program` is required: the file of Cut to debug" });
        };
        return match Session::launch_cut(Path::new(program)) {
            Err(why) => json!({ "error": why }),
            Ok(mut s) => {
                let stop = s.stand_at_entry();
                *held = Some(Held(s));
                match stop {
                    Some(stop) => json!({ "stop": stop_json(&stop) }),
                    None => json!({}),
                }
            }
        };
    }
    if what == "end" {
        *held = None;
        return json!({});
    }
    let Some(Held(s)) = held.as_mut() else {
        return json!({ "error": "nothing is being debugged: launch first" });
    };
    match what {
        "breakpoints" => {
            let path = req["path"].as_str().unwrap_or_default();
            let lines: Vec<u32> = req["lines"]
                .as_array()
                .map(|ls| {
                    ls.iter()
                        .filter_map(|l| l.as_u64())
                        .map(|l| l as u32)
                        .collect()
                })
                .unwrap_or_default();
            json!({ "lines": s.set_breakpoints(Path::new(path), &lines) })
        }
        "resume" => {
            let mode = match req["mode"].as_str() {
                Some("stepIn") => Mode::StepIn,
                Some("stepOver") => Mode::StepOver,
                Some("stepOut") => Mode::StepOut,
                _ => Mode::Continue,
            };
            s.resume(mode);
            json!({})
        }
        "run" => {
            let budget = req["budget"].as_u64().unwrap_or(20_000);
            let stop = s.run(budget);
            json!({
                "stop": stop.as_ref().map(stop_json),
                "output": s.take_output(),
            })
        }
        "frames" => {
            let max = req["max"].as_u64().unwrap_or(1000) as usize;
            let frames: Vec<Json> = s
                .frames(max)
                .iter()
                .map(|f| {
                    let mut row = json!({ "name": f.name, "path": "", "line": 0, "column": 0 });
                    if let Some((file, loc)) = f.loc.and_then(|l| s.file(l).map(|file| (file, l))) {
                        let (line, column) = file.position(loc.span.start);
                        row["line"] = json!(line);
                        row["column"] = json!(column);
                        row["path"] = json!(match &file.path {
                            Some(p) => super::session::plain_path(&p.display().to_string()),
                            None => file.name.clone(),
                        });
                    }
                    row
                })
                .collect();
            json!({ "frames": frames })
        }
        "scopes" => {
            let frame = req["frame"].as_u64().unwrap_or(0) as usize;
            let scopes: Vec<Json> = s
                .scopes(frame)
                .into_iter()
                .map(|sc| {
                    json!({ "name": sc.name, "reference": sc.reference, "expensive": sc.expensive })
                })
                .collect();
            json!({ "scopes": scopes })
        }
        "variables" => {
            let reference = req["reference"].as_u64().unwrap_or(0) as u32;
            let rows: Vec<Json> = s.variables(reference).iter().map(row_json).collect();
            json!({ "variables": rows })
        }
        "evaluate" => {
            let frame = req["frame"].as_u64().unwrap_or(0) as usize;
            let expr = req["expression"].as_str().unwrap_or_default();
            json!({ "variable": s.evaluate(frame, expr).as_ref().map(row_json) })
        }
        "output" => json!({ "output": s.take_output() }),
        other => json!({ "error": format!("`{other}` is not something a session does") }),
    }
}

fn row_json(v: &Variable) -> Json {
    json!({
        "name": v.name,
        "value": v.value,
        "type": v.ty.clone().unwrap_or_default(),
        "reference": v.children,
    })
}

fn stop_json(stop: &Stop) -> Json {
    let (reason, text) = match stop {
        Stop::Entry => ("entry", String::new()),
        Stop::Breakpoint => ("breakpoint", String::new()),
        Stop::Step => ("step", String::new()),
        Stop::Pause => ("pause", String::new()),
        Stop::Exception(why) => ("exception", why.clone()),
        Stop::Exited(Ok(value)) => ("exited", value.clone()),
        Stop::Exited(Err(why)) => ("failed", why.clone()),
    };
    json!({ "reason": reason, "text": text })
}
