//! `meadow dap` — a debug adapter, speaking the Debug Adapter Protocol over
//! stdin and stdout.
//!
//! An editor starts it, sends `launch` with a program to debug, and from then
//! on asks it to run, stop, step and describe what it sees. The debugging
//! itself is [`session::Session`]; this module is the protocol and the loop.
//!
//! # Running without blocking
//!
//! While the program runs, the adapter still has to answer: a `pause` has to
//! land, and an editor asks for threads and sets breakpoints whenever it likes.
//! So stdin is read on a thread of its own, and the program runs in slices of
//! [`SLICE`] instructions with the queue checked between them. A slice is small
//! enough that a pause feels immediate and large enough that checking costs
//! nothing measurable.
//!
//! # Output
//!
//! The program's `print` and `println` would otherwise go to stdout, which is
//! the protocol. They are captured and forwarded as `output` events, which is
//! where an editor shows a debuggee's console; `Console.readLine` sees the end
//! of input.

pub mod session;
pub mod silo;

use serde_json::{Value as Json, json};
use session::{Mode, Session, Stop};

/// What is being debugged: a program on Glade's VM, in this process, or on
/// Silo's interpreter, in its own (`silo::SiloSession`). The adapter asks
/// both the same things.
pub enum Debuggee {
    Glade(Session),
    Silo(silo::SiloSession),
}

/// `Debuggee::$name`, of whichever it is.
macro_rules! either {
    ($self:ident, $s:ident => $e:expr) => {
        match $self {
            Debuggee::Glade($s) => $e,
            Debuggee::Silo($s) => $e,
        }
    };
}

impl Debuggee {
    fn run(&mut self, budget: u64) -> Option<Stop> {
        either!(self, s => s.run(budget))
    }
    fn take_output(&mut self) -> String {
        either!(self, s => s.take_output())
    }
    fn is_finished(&self) -> bool {
        either!(self, s => s.is_finished())
    }
    fn set_breakpoints(&mut self, path: &std::path::Path, lines: &[u32]) -> Vec<Option<u32>> {
        either!(self, s => s.set_breakpoints(path, lines))
    }
    fn resume(&mut self, mode: Mode) {
        either!(self, s => s.resume(mode))
    }
    fn resume_until_in(&mut self, name: &str, module: &std::path::Path) -> bool {
        either!(self, s => s.resume_until_in(name, module))
    }
    fn stop_at_entry(&mut self) -> Option<Stop> {
        either!(self, s => s.stop_at_entry())
    }
    fn frames(&mut self, max: usize) -> Vec<session::Frame> {
        either!(self, s => s.frames(max))
    }
    fn file(&self, loc: meadow_compiler::core::Loc) -> Option<&session::SourceFile> {
        either!(self, s => s.file(loc))
    }
    fn scopes(&mut self, frame: usize) -> Vec<session::Scope> {
        either!(self, s => s.scopes(frame))
    }
    fn variables(&mut self, reference: u32) -> Vec<session::Variable> {
        either!(self, s => s.variables(reference))
    }
    fn evaluate(&mut self, frame: usize, expr: &str) -> Option<session::Variable> {
        either!(self, s => s.evaluate(frame, expr))
    }
    /// Stop the running program. Glade's is run a slice at a time, and is
    /// stopped by not running another; Silo's runs on its own, and is told.
    fn pause(&mut self) -> Option<Stop> {
        match self {
            Debuggee::Glade(_) => Some(Stop::Pause),
            Debuggee::Silo(s) => s.pause(),
        }
    }
}
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;

/// Instructions run between looks at the request queue.
const SLICE: u64 = 20_000;

/// The single thread a Meadow program has.
const THREAD: i64 = 1;

/// Serve one debugging session on stdin and stdout, until the editor
/// disconnects.
pub fn run() -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        while let Some(msg) = read_message(&mut input) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    let stdout = std::io::stdout();
    let mut adapter = Adapter::new(Box::new(stdout.lock()));
    adapter.serve(rx);
    Ok(())
}

/// Read one `Content-Length`-framed message.
pub fn read_message(input: &mut impl BufRead) -> Option<Json> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            length = v.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

pub struct Adapter {
    out: Box<dyn Write>,
    seq: i64,
    session: Option<Debuggee>,
    /// Breakpoints the editor set before the program was built, by file.
    pending_breakpoints: Vec<(PathBuf, Vec<u32>)>,
    stop_on_entry: bool,
    /// A function to stop in as soon as it is entered, and the file it is in.
    stop_in: Option<(String, PathBuf)>,
    running: bool,
    done: bool,
    lines_from_1: bool,
    columns_from_1: bool,
}

impl Adapter {
    pub fn new(out: Box<dyn Write>) -> Adapter {
        Adapter {
            out,
            seq: 1,
            session: None,
            pending_breakpoints: Vec::new(),
            stop_on_entry: false,
            stop_in: None,
            running: false,
            done: false,
            lines_from_1: true,
            columns_from_1: true,
        }
    }

    /// Handle messages from `rx`, running the program between them, until
    /// the editor says goodbye or goes away.
    pub fn serve(&mut self, rx: mpsc::Receiver<Json>) {
        while !self.done {
            let msg = if self.running {
                match rx.try_recv() {
                    Ok(m) => Some(m),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => break,
                }
            };
            if let Some(m) = msg {
                self.handle(m);
            }
            if self.running {
                self.slice();
            }
        }
    }

    /// Run until the program stops or finishes -- for a caller driving
    /// [`Adapter::handle`] by hand rather than through [`Adapter::serve`].
    pub fn settle(&mut self) {
        while self.running {
            self.slice();
        }
    }

    /// Run one slice, and report whatever it stopped for.
    fn slice(&mut self) {
        let Some(s) = &mut self.session else {
            self.running = false;
            return;
        };
        let stop = s.run(SLICE);
        self.flush_output();
        if let Some(stop) = stop {
            self.running = false;
            self.report(stop);
        }
    }

    fn flush_output(&mut self) {
        let Some(s) = &mut self.session else { return };
        let text = s.take_output();
        if !text.is_empty() {
            self.event("output", json!({ "category": "stdout", "output": text }));
        }
    }

    fn report(&mut self, stop: Stop) {
        let (reason, text) = match stop {
            Stop::Entry => ("entry", None),
            Stop::Breakpoint => ("breakpoint", None),
            Stop::Step => ("step", None),
            Stop::Pause => ("pause", None),
            Stop::Exception(msg) => ("exception", Some(msg)),
            Stop::Exited(result) => {
                // `main` gives back `()`, which says nothing; an entry
                // expression's value is what it was run for.
                let (line, code) = match result {
                    Ok(v) if v == "()" => (String::new(), 0),
                    Ok(v) => (format!("=> {v}\n"), 0),
                    Err(e) => (format!("{e}\n"), 1),
                };
                let category = if code == 0 { "console" } else { "stderr" };
                if !line.is_empty() {
                    self.event("output", json!({ "category": category, "output": line }));
                }
                self.event("exited", json!({ "exitCode": code }));
                self.event("terminated", json!({}));
                return;
            }
        };
        let mut body = json!({
            "reason": reason,
            "threadId": THREAD,
            "allThreadsStopped": true,
        });
        if let Some(t) = text {
            body["description"] = json!("Runtime error");
            body["text"] = json!(t);
        }
        self.event("stopped", body);
    }

    // --- messages -----------------------------------------------------------

    fn send(&mut self, mut msg: Json) {
        msg["seq"] = json!(self.seq);
        self.seq += 1;
        let body = msg.to_string();
        let _ = write!(self.out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        let _ = self.out.flush();
    }

    fn event(&mut self, event: &str, body: Json) {
        self.send(json!({ "type": "event", "event": event, "body": body }));
    }

    fn respond(&mut self, req: &Json, body: Json) {
        self.send(json!({
            "type": "response",
            "request_seq": req["seq"],
            "command": req["command"],
            "success": true,
            "body": body,
        }));
    }

    fn fail(&mut self, req: &Json, message: impl Into<String>) {
        let message = message.into();
        self.send(json!({
            "type": "response",
            "request_seq": req["seq"],
            "command": req["command"],
            "success": false,
            "message": message,
            "body": { "error": { "id": 1, "format": message, "showUser": true } },
        }));
    }

    pub fn handle(&mut self, req: Json) {
        if req["type"] != "request" {
            return;
        }
        let args = req["arguments"].clone();
        match req["command"].as_str().unwrap_or("") {
            "initialize" => {
                self.lines_from_1 = args["linesStartAt1"].as_bool().unwrap_or(true);
                self.columns_from_1 = args["columnsStartAt1"].as_bool().unwrap_or(true);
                self.respond(
                    &req,
                    json!({
                        "supportsConfigurationDoneRequest": true,
                        "supportsEvaluateForHovers": true,
                        "supportsTerminateRequest": true,
                    }),
                );
            }

            "launch" => {
                let Some(program) = args["program"].as_str() else {
                    return self.fail(
                        &req,
                        "`program` is required: a .mw file or a package directory",
                    );
                };
                self.stop_on_entry = args["stopOnEntry"].as_bool().unwrap_or(false);
                // `entry`: start at an expression instead of `main`, and stop
                // when the function it names is entered.
                let entry = match (
                    args["entry"]["module"].as_str(),
                    args["entry"]["expression"].as_str(),
                ) {
                    (Some(module), Some(expression)) => Some(session::Entry {
                        module: PathBuf::from(module),
                        expression: expression.to_string(),
                    }),
                    _ => None,
                };
                self.stop_in = match (args["entry"]["function"].as_str(), &entry) {
                    (Some(f), Some(e)) => Some((f.to_string(), e.module.clone())),
                    _ => None,
                };
                // Silo's interpreter, when the launch or the package's debug
                // profile asks for Silo; Glade's VM otherwise.
                let path = std::path::Path::new(program);
                let silo = match args["runtime"].as_str() {
                    Some(r) => r == "silo",
                    None => {
                        crate::profile::Resolved::resolve(
                            crate::profile::Profile::Debug,
                            path,
                            Default::default(),
                        )
                        .runtime
                            == crate::aot::Runtime::Silo
                    }
                };
                let launched = if silo {
                    silo::SiloSession::launch_at(path, entry.as_ref()).map(Debuggee::Silo)
                } else {
                    Session::launch_at(path, entry.as_ref()).map(Debuggee::Glade)
                };
                match launched {
                    Ok(s) => {
                        self.session = Some(s);
                        for (path, lines) in std::mem::take(&mut self.pending_breakpoints) {
                            if let Some(s) = &mut self.session {
                                s.set_breakpoints(&path, &lines);
                            }
                        }
                        self.respond(&req, json!({}));
                        self.event("initialized", json!({}));
                    }
                    Err(e) => {
                        self.event(
                            "output",
                            json!({ "category": "stderr", "output": format!("{e}\n") }),
                        );
                        self.fail(&req, e);
                    }
                }
            }

            "setBreakpoints" => {
                let Some(path) = args["source"]["path"].as_str() else {
                    return self.respond(&req, json!({ "breakpoints": [] }));
                };
                let lines: Vec<u32> = args["breakpoints"]
                    .as_array()
                    .map(|bs| {
                        bs.iter()
                            .filter_map(|b| b["line"].as_u64())
                            .map(|l| self.line_in(l as u32))
                            .collect()
                    })
                    .unwrap_or_default();
                let path = PathBuf::from(path);
                let landed = match &mut self.session {
                    Some(s) => s.set_breakpoints(&path, &lines),
                    None => {
                        self.pending_breakpoints.retain(|(p, _)| *p != path);
                        self.pending_breakpoints.push((path, lines.clone()));
                        lines.iter().map(|l| Some(*l)).collect()
                    }
                };
                let breakpoints: Vec<Json> = landed
                    .into_iter()
                    .map(|l| match l {
                        Some(line) => json!({ "verified": true, "line": self.line_out(line) }),
                        None => json!({ "verified": false, "message": "no code on or just after this line" }),
                    })
                    .collect();
                self.respond(&req, json!({ "breakpoints": breakpoints }));
            }

            "setExceptionBreakpoints" => self.respond(&req, json!({})),

            "configurationDone" => {
                self.respond(&req, json!({}));
                let Some(s) = &mut self.session else { return };
                if let Some((name, module)) = self.stop_in.clone() {
                    if !s.resume_until_in(&name, &module) {
                        s.resume(Mode::Continue);
                    }
                    self.running = true;
                } else if self.stop_on_entry {
                    let stop = s.stop_at_entry();
                    self.flush_output();
                    if let Some(stop) = stop {
                        self.report(stop);
                    }
                } else {
                    s.resume(Mode::Continue);
                    self.running = true;
                }
            }

            "threads" => self.respond(
                &req,
                json!({ "threads": [{ "id": THREAD, "name": "main" }] }),
            ),

            "stackTrace" => {
                let start = args["startFrame"].as_u64().unwrap_or(0) as usize;
                let levels = match args["levels"].as_u64() {
                    Some(0) | None => 1000,
                    Some(n) => n as usize,
                };
                let (lines_from_1, columns_from_1) = (self.lines_from_1, self.columns_from_1);
                let line_out = |l: u32| if lines_from_1 { l } else { l - 1 };
                let column_out = |c: u32| if columns_from_1 { c } else { c - 1 };
                let Some(s) = &mut self.session else {
                    return self.respond(&req, json!({ "stackFrames": [], "totalFrames": 0 }));
                };
                let frames = s.frames(start + levels);
                let total = frames.len();
                let rows: Vec<Json> = frames
                    .iter()
                    .enumerate()
                    .skip(start)
                    .map(|(i, f)| {
                        let mut row = json!({ "id": i, "name": f.name, "line": 0, "column": 0 });
                        if let Some((file, loc)) =
                            f.loc.and_then(|l| s.file(l).map(|file| (file, l)))
                        {
                            let (line, col) = file.position(loc.span.start);
                            row["line"] = json!(line_out(line));
                            row["column"] = json!(column_out(col));
                            let (end_line, end_col) = file.position(loc.span.end);
                            row["endLine"] = json!(line_out(end_line));
                            row["endColumn"] = json!(column_out(end_col));
                            row["source"] = source_json(file);
                        }
                        row
                    })
                    .collect();
                self.respond(&req, json!({ "stackFrames": rows, "totalFrames": total }));
            }

            "scopes" => {
                let frame = args["frameId"].as_u64().unwrap_or(0) as usize;
                let Some(s) = &mut self.session else {
                    return self.respond(&req, json!({ "scopes": [] }));
                };
                let scopes: Vec<Json> = s
                    .scopes(frame)
                    .into_iter()
                    .map(|sc| {
                        json!({
                            "name": sc.name,
                            "variablesReference": sc.reference,
                            "expensive": sc.expensive,
                            "presentationHint": if sc.name == "Locals" { "locals" } else { "registers" },
                        })
                    })
                    .collect();
                self.respond(&req, json!({ "scopes": scopes }));
            }

            "variables" => {
                let reference = args["variablesReference"].as_u64().unwrap_or(0) as u32;
                let Some(s) = &mut self.session else {
                    return self.respond(&req, json!({ "variables": [] }));
                };
                let rows: Vec<Json> = s
                    .variables(reference)
                    .into_iter()
                    .map(|v| {
                        let mut row = json!({
                            "name": v.name,
                            "value": v.value,
                            "variablesReference": v.children,
                        });
                        if let Some(ty) = v.ty.filter(|t| !t.is_empty()) {
                            row["type"] = json!(ty);
                        }
                        row
                    })
                    .collect();
                self.respond(&req, json!({ "variables": rows }));
            }

            "evaluate" => {
                let frame = args["frameId"].as_u64().unwrap_or(0) as usize;
                let expr = args["expression"].as_str().unwrap_or("");
                let found = self.session.as_mut().and_then(|s| s.evaluate(frame, expr));
                match found {
                    Some(v) => {
                        let mut body =
                            json!({ "result": v.value, "variablesReference": v.children });
                        if let Some(ty) = v.ty.filter(|t| !t.is_empty()) {
                            body["type"] = json!(ty);
                        }
                        self.respond(&req, body)
                    }
                    None => self.fail(&req, format!("`{expr}` is not a name in scope here")),
                }
            }

            command @ ("continue" | "next" | "stepIn" | "stepOut") => {
                let mode = match command {
                    "continue" => Mode::Continue,
                    "next" => Mode::StepOver,
                    "stepIn" => Mode::StepIn,
                    _ => Mode::StepOut,
                };
                let Some(s) = &mut self.session else {
                    return self.fail(&req, "nothing is running");
                };
                if s.is_finished() {
                    return self.fail(
                        &req,
                        "the program has stopped for good: it failed or finished",
                    );
                }
                s.resume(mode);
                self.running = true;
                if command == "continue" {
                    self.respond(&req, json!({ "allThreadsContinued": true }));
                } else {
                    self.respond(&req, json!({}));
                }
            }

            "pause" => {
                self.respond(&req, json!({}));
                if self.running {
                    self.running = false;
                    let stop = self.session.as_mut().and_then(Debuggee::pause);
                    self.flush_output();
                    if let Some(stop) = stop {
                        self.report(stop);
                    }
                }
            }

            "disconnect" | "terminate" => {
                self.respond(&req, json!({}));
                if req["command"] == "terminate" {
                    self.event("terminated", json!({}));
                }
                self.running = false;
                self.done = req["command"] == "disconnect";
            }

            other => self.fail(&req, format!("`{other}` is not supported")),
        }
    }

    fn line_in(&self, line: u32) -> u32 {
        if self.lines_from_1 { line } else { line + 1 }
    }

    fn line_out(&self, line: u32) -> u32 {
        if self.lines_from_1 { line } else { line - 1 }
    }
}

fn source_json(file: &session::SourceFile) -> Json {
    match &file.path {
        Some(p) => json!({
            "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            "path": session::plain_path(&p.display().to_string()),
        }),
        None => json!({ "name": file.name }),
    }
}
