//! One debugging session on **Silo's interpreter**: the program built as for
//! Glade's session ([`session::build`]), written as an image, and run by
//! `meadow-silo-vm --debug` in a process of its own, which this drives over a
//! connection of its own (`silo/vm/src/interp/debug.rs` says what is said on
//! it). The VM's standard output is the program's, forwarded as Glade's
//! session forwards what its program prints.
//!
//! What a person sees is worked out here, as Glade's session works it out:
//! which source and line each instruction is on, where a step may stop (a
//! *boundary*, the first instruction of a source position), which pcs a
//! breakpoint on a line means, and what each variable is called. The VM says
//! where it is and what its frames hold.

use super::session::{
    self, Entry, Frame, LaunchError, Mode, Scope, SourceFile, Stop, VarInfo, Variable,
};
use meadow_compiler::core::Loc;
use serde_json::{Value as Json, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A procedure of the VM's: what it is of, and where its code is.
struct Proc {
    name: String,
    entry: usize,
    end: usize,
    /// Where the `new` that makes it is, for one a `new` makes.
    origin: Option<usize>,
}

/// A variable a frame holds, as the VM showed it.
struct Shown {
    var: u32,
    value: String,
    handle: u64,
}

struct Stacked {
    proc: usize,
    pc: usize,
    vars: Vec<Shown>,
}

/// What a variables reference stands for, until the program runs again.
enum Target {
    Locals(usize),
    Value(u64),
}

pub struct SiloSession {
    child: Child,
    out: TcpStream,
    input: BufReader<TcpStream>,
    procs: Vec<Proc>,
    /// Where each instruction was written.
    locs: Vec<Option<Loc>>,
    pub files: HashMap<u32, SourceFile>,
    vars: HashMap<u32, VarInfo>,
    boundary: Vec<bool>,
    /// Breakpoints, by the file they are in: every pc the VM is told of.
    breakpoints: HashMap<std::path::PathBuf, Vec<usize>>,
    /// What has arrived of a line the VM is still saying.
    pending: String,
    output: Arc<Mutex<String>>,
    /// Whether the program's output has ended: it has, when it has exited.
    output_ended: Arc<AtomicBool>,
    /// Whether the VM has been told to begin.
    begun: bool,
    finished: Option<Stop>,
    stack: Vec<Stacked>,
    refs: Vec<Target>,
}

impl SiloSession {
    /// Build `path` for debugging, starting at `entry` if there is one, and
    /// stand it at its start on the VM, not yet run.
    pub fn launch_at(path: &Path, entry: Option<&Entry>) -> Result<SiloSession, LaunchError> {
        SiloSession::launch_with(path, entry, None)
    }

    /// [`SiloSession::launch_at`], its frames kept as `stack` says: on the
    /// heap, as Glade's are, or on a native stack that grows -- which is
    /// what it is unless the launch or `MEADOW_STACK` says `heap`. A fixed
    /// stack is never what a debugger wants.
    pub fn launch_with(
        path: &Path,
        entry: Option<&Entry>,
        stack: Option<meadow_llvm::Stack>,
    ) -> Result<SiloSession, LaunchError> {
        let began = Instant::now();
        let say = |what: &str| {
            if std::env::var_os("MEADOW_DAP_TIMES").is_some() {
                eprintln!("dap: {what} at {:?}", began.elapsed());
            }
        };
        let (program, packages) = session::build(path, entry)?;
        say("built");
        let origins: Vec<(u32, u32)> = program.origins.iter().map(|(c, o)| (c.0, o.0)).collect();
        let (files, vars) = session::sources(&packages, origins);
        let dir = std::env::temp_dir()
            .join("meadow-silo-debug")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // One image a session: a process may hold several, as tests do.
        static SESSIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let image = dir.join(format!(
            "debugged-{}.image",
            SESSIONS.fetch_add(1, Ordering::Relaxed)
        ));
        // A debugger's program counts as the build would, and its stack grows
        // as Glade's does: a deep recursion is something to look at, not a
        // reason to stop.
        let asked = stack.or_else(|| {
            std::env::var("MEADOW_STACK")
                .ok()
                .and_then(|s| meadow_llvm::Stack::named(&s))
        });
        let config = meadow_llvm::Config {
            memory: crate::aot::gc(),
            stack: match asked {
                Some(meadow_llvm::Stack::Heap) => meadow_llvm::Stack::Heap,
                _ => meadow_llvm::Stack::Growable,
            },
        };
        crate::aot::write_image_with(&image, program, Vec::new(), config)?;
        say("image written");

        let listener =
            TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("cannot listen: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let mut child = Command::new(crate::aot::interpreter()?)
            .arg("--debug")
            .arg(port.to_string())
            .arg(&image)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start Silo's interpreter: {e}"))?;
        let output = Arc::new(Mutex::new(String::new()));
        let output_ended = Arc::new(AtomicBool::new(false));
        for stream in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        .enumerate()
        {
            let (i, mut stream) = stream;
            let sink = output.clone();
            let ended = output_ended.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    sink.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push_str(&String::from_utf8_lossy(&buf[..n]));
                }
                if i == 0 {
                    ended.store(true, Ordering::Relaxed);
                }
            });
        }
        let conn = accept(&listener, &mut child)?;
        say("connected");
        let mut input = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
        let ready = read_line(&mut input).ok_or("Silo's interpreter said nothing")?;
        let ready = &ready["ready"];
        say("ready");
        let n = ready["code"].as_u64().unwrap_or(0) as usize;
        let procs: Vec<Proc> = ready["procs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| Proc {
                name: p["name"].as_str().unwrap_or("?").to_string(),
                entry: p["entry"].as_u64().unwrap_or(0) as usize,
                end: p["end"].as_u64().unwrap_or(0) as usize,
                origin: p["origin"].as_u64().map(|o| o as usize),
            })
            .collect();
        let mut locs = vec![None; n];
        for l in ready["locs"].as_array().into_iter().flatten() {
            let (Some(pc), Some(source), Some(start), Some(end)) =
                (l[0].as_u64(), l[1].as_u64(), l[2].as_u64(), l[3].as_u64())
            else {
                continue;
            };
            if let Some(slot) = locs.get_mut(pc as usize) {
                *slot = Some(Loc {
                    source: source as u32,
                    span: meadow_compiler::span::Span::new(start as u32, end as u32),
                });
            }
        }
        let mut s = SiloSession {
            child,
            out: conn,
            input,
            procs,
            boundary: vec![false; n],
            locs,
            files,
            vars,
            breakpoints: HashMap::new(),
            pending: String::new(),
            output,
            output_ended,
            begun: false,
            finished: None,
            stack: Vec::new(),
            refs: Vec::new(),
        };
        s.boundary = s.boundaries();
        let keys: Vec<Json> = (0..n)
            .filter_map(|pc| s.key(pc).map(|k| json!([pc, k])))
            .collect();
        let boundary: Vec<usize> = (0..n).filter(|pc| s.boundary[*pc]).collect();
        s.send(json!({ "setup": { "boundary": boundary, "keys": keys } }));
        say("set up");
        Ok(s)
    }

    /// Where a step may stop: the first instruction of a source position in
    /// each procedure -- but not where a frame's code begins, which is still
    /// in the position of the call that made it, and was stopped at on the
    /// way in.
    fn boundaries(&self) -> Vec<bool> {
        let mut out = vec![false; self.locs.len()];
        for p in &self.procs {
            let frame = p
                .name
                .rsplit('/')
                .next()
                .is_some_and(|last| last.starts_with("frame"));
            for pc in p.entry..p.end.min(self.locs.len()) {
                let Some(here) = self.locs[pc] else { continue };
                out[pc] = if pc == p.entry {
                    !frame
                } else {
                    self.locs[pc - 1] != Some(here)
                };
            }
        }
        out
    }

    /// Which line of which source `pc` is on, as one number.
    fn key(&self, pc: usize) -> Option<u64> {
        let loc = self.locs.get(pc).copied().flatten()?;
        let line = self.files.get(&loc.source)?.line_of(loc.span.start);
        Some((u64::from(loc.source) << 32) | u64::from(line))
    }

    /// One write a message, as the VM sends them.
    fn send(&mut self, msg: Json) {
        let mut text = msg.to_string();
        text.push('\n');
        let _ = self.out.write_all(text.as_bytes());
        let _ = self.out.flush();
    }

    /// The next thing the VM says, waiting at most `wait`. A line that is
    /// only partly there when the wait ends is kept for the next look.
    fn hear(&mut self, wait: Option<Duration>) -> Option<Json> {
        let _ = self.input.get_ref().set_read_timeout(wait);
        loop {
            match self.input.read_line(&mut self.pending) {
                Ok(0) => return None,
                Ok(_) if self.pending.ends_with('\n') => {
                    let line = std::mem::take(&mut self.pending);
                    if let Ok(msg) = serde_json::from_str(&line) {
                        return Some(msg);
                    }
                }
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    pub fn take_output(&mut self) -> String {
        std::mem::take(&mut *self.output.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn is_finished(&self) -> bool {
        self.finished.is_some()
    }

    pub fn file(&self, loc: Loc) -> Option<&SourceFile> {
        self.files.get(&loc.source)
    }

    // --- breakpoints ------------------------------------------------------

    /// As [`session::Session::set_breakpoints`]: the first boundary of each
    /// line in each procedure, on the line asked for or a little after it.
    pub fn set_breakpoints(&mut self, path: &Path, lines: &[u32]) -> Vec<Option<u32>> {
        let wanted = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let ids: HashSet<u32> = self
            .files
            .iter()
            .filter(|(_, f)| {
                f.path.as_ref().is_some_and(|p| {
                    std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == wanted
                })
            })
            .map(|(id, _)| *id)
            .collect();
        let mut by_line: HashMap<u32, Vec<usize>> = HashMap::new();
        for p in &self.procs {
            let mut seen = HashSet::new();
            for pc in p.entry..p.end.min(self.locs.len()) {
                let Some(loc) = self.locs[pc].filter(|_| self.boundary[pc]) else {
                    continue;
                };
                if !ids.contains(&loc.source) {
                    continue;
                }
                let line = self.files[&loc.source].line_of(loc.span.start);
                if seen.insert(line) {
                    by_line.entry(line).or_default().push(pc);
                }
            }
        }
        let mut here = Vec::new();
        let landed = lines
            .iter()
            .map(|&line| {
                let (landed, pcs) =
                    (line..line + 3).find_map(|l| by_line.get(&l).map(|p| (l, p)))?;
                here.extend(pcs.iter().copied());
                Some(landed)
            })
            .collect();
        self.breakpoints.insert(wanted, here);
        let all: Vec<usize> = self.breakpoints.values().flatten().copied().collect();
        self.send(json!({ "breakpoints": all }));
        landed
    }

    // --- running ----------------------------------------------------------

    pub fn resume(&mut self, mode: Mode) {
        self.refs.clear();
        self.stack.clear();
        let run = match mode {
            Mode::Continue => json!({ "mode": "continue" }),
            Mode::StepIn => json!({ "mode": "in" }),
            Mode::StepOver => json!({ "mode": "over" }),
            Mode::StepOut => json!({ "mode": "out" }),
            Mode::Enter(name, source) => {
                let procs = self.procs_of(&name, source);
                json!({ "mode": "enter", "procs": procs })
            }
        };
        self.begun = true;
        self.send(json!({ "run": run }));
    }

    /// The procedures of the top-level definition `name` in source `source`:
    /// its own, and everything made inside it.
    fn procs_of(&self, name: &str, source: u32) -> Vec<usize> {
        self.procs
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                let def = p.name.split('/').next().unwrap_or("");
                def.rsplit('.').next() == Some(name)
                    && (p.entry..p.end.min(self.locs.len()))
                        .any(|pc| self.locs[pc].is_some_and(|l| l.source == source))
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn resume_until_in(&mut self, name: &str, module: &Path) -> bool {
        let want = std::fs::canonicalize(module).unwrap_or_else(|_| module.to_path_buf());
        let source = self.files.iter().find_map(|(id, f)| {
            let p = f.path.as_ref()?;
            (std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == want).then_some(*id)
        });
        let Some(source) = source else { return false };
        self.resume(Mode::Enter(name.into(), source));
        true
    }

    pub fn stop_at_entry(&mut self) -> Option<Stop> {
        self.resume(Mode::StepIn);
        match self.wait(None) {
            Some(Stop::Step) => Some(Stop::Entry),
            other => other,
        }
    }

    /// Ask the running program to stop, and wait until it has.
    pub fn pause(&mut self) -> Option<Stop> {
        if self.finished.is_some() || !self.begun {
            return None;
        }
        self.send(json!({ "pause": true }));
        self.wait(None)
    }

    /// What the VM says next: a stop, or nothing within `wait`.
    fn wait(&mut self, wait: Option<Duration>) -> Option<Stop> {
        if let Some(done) = &self.finished {
            return Some(done.clone());
        }
        let msg = match self.hear(wait) {
            Some(m) => m,
            None if wait.is_some() && self.child.try_wait().ok().flatten().is_none() => {
                return None;
            }
            // The VM is gone without saying how: it ended.
            None => return Some(self.ended(Ok("()".into()))),
        };
        if let Some(reason) = msg["stopped"].as_str() {
            return Some(match reason {
                "breakpoint" => Stop::Breakpoint,
                "pause" => Stop::Pause,
                "entry" => Stop::Entry,
                _ => Stop::Step,
            });
        }
        if let Some(e) = msg["exception"].as_str() {
            self.finished = Some(Stop::Exited(Err(e.to_string())));
            return Some(Stop::Exception(e.to_string()));
        }
        if let Some(x) = msg.get("exited") {
            let shown = x["ok"].as_str().unwrap_or("()").to_string();
            return Some(self.ended(Ok(shown)));
        }
        None
    }

    /// The program finished: its output, all of it, before it says so.
    fn ended(&mut self, result: Result<String, String>) -> Stop {
        let _ = self.child.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.output_ended.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let done = Stop::Exited(result);
        self.finished = Some(done.clone());
        done
    }

    /// As [`session::Session::run`]: `None` while it is still going. The
    /// budget is not instructions here but how long to wait for news.
    pub fn run(&mut self, _budget: u64) -> Option<Stop> {
        self.wait(Some(Duration::from_millis(20)))
    }

    // --- looking ----------------------------------------------------------

    /// The frames of the stopped program, asked for once a stop. A failed
    /// program is stopped too, until it is let go; a finished one has none.
    fn stacked(&mut self) -> &[Stacked] {
        let gone = matches!(self.finished, Some(Stop::Exited(Ok(_))));
        if self.stack.is_empty() && !gone {
            self.send(json!({ "stack": 1000 }));
            let frames = self
                .hear(Some(Duration::from_secs(10)))
                .map(|m| m["stack"].clone())
                .unwrap_or(Json::Null);
            self.stack = frames
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| Stacked {
                    proc: f["proc"].as_u64().unwrap_or(0) as usize,
                    pc: f["pc"].as_u64().unwrap_or(0) as usize,
                    vars: f["vars"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|v| Shown {
                            var: v[0].as_u64().unwrap_or(0) as u32,
                            value: v[1].as_str().unwrap_or("").to_string(),
                            handle: v[2].as_u64().unwrap_or(0),
                        })
                        .collect(),
                })
                .collect();
        }
        &self.stack
    }

    /// Where `pc` was written, or failing that the nearest instruction before
    /// it in its procedure that says.
    fn loc_near(&self, pc: usize) -> Option<Loc> {
        let p = self.procs.iter().find(|p| p.entry <= pc && pc < p.end)?;
        (p.entry..=pc.min(self.locs.len().saturating_sub(1)))
            .rev()
            .find_map(|i| self.locs[i])
    }

    /// The frames a person wrote: what the compiler made to start the
    /// program with (`.start`, named so no one can write it) is not one.
    fn shown_frames(&mut self) -> Vec<usize> {
        let procs = &self.procs;
        let rows: Vec<usize> = self.stack.iter().map(|f| f.proc).collect();
        rows.iter()
            .enumerate()
            .filter(|(_, p)| !procs.get(**p).is_some_and(|p| p.name.starts_with('.')))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn frames(&mut self, max: usize) -> Vec<Frame> {
        self.stacked();
        let shown = self.shown_frames();
        let rows: Vec<(usize, usize)> = shown
            .iter()
            .map(|i| (self.stack[*i].proc, self.stack[*i].pc))
            .collect();
        rows.into_iter()
            .take(max)
            .map(|(proc, pc)| Frame {
                // The definition's own name, as Glade's frames give it: not
                // its module's, nor which of its methods or frames this is.
                name: session::display_name(self.procs.get(proc).map_or("?", |p| {
                    let def = p.name.split('/').next().unwrap_or("?");
                    def.rsplit('.').next().unwrap_or(def)
                })),
                // A frame waiting on the heap is where the call that made
                // its continuation is, as Glade's frames show it.
                loc: match self.procs.get(proc) {
                    Some(p) if pc == p.entry && p.origin.is_some() => {
                        p.origin.and_then(|o| self.loc_near(o))
                    }
                    _ => self.loc_near(pc),
                },
                vars: Vec::new(),
            })
            .collect()
    }

    pub fn scopes(&mut self, frame: usize) -> Vec<Scope> {
        self.refs.push(Target::Locals(frame));
        vec![Scope {
            name: "Locals",
            reference: self.refs.len() as u32,
            expensive: false,
        }]
    }

    fn reference(&mut self, handle: u64) -> u32 {
        if handle == 0 {
            return 0;
        }
        self.refs.push(Target::Value(handle));
        self.refs.len() as u32
    }

    /// The named variables of frame `frame`, as Glade's session shows them:
    /// those a person wrote, under the names they wrote.
    fn locals(&mut self, frame: usize) -> Vec<Variable> {
        self.stacked();
        // Frame `frame` of those shown, not of all the VM has.
        let Some(frame) = self.shown_frames().get(frame).copied() else {
            return Vec::new();
        };
        let rows: Vec<(u32, String, u64)> = self
            .stack
            .get(frame)
            .map(|f| {
                f.vars
                    .iter()
                    .map(|v| (v.var, v.value.clone(), v.handle))
                    .collect()
            })
            .unwrap_or_default();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (var, value, handle) in rows {
            let Some(info) = self.vars.get(&var) else {
                continue;
            };
            let (name, ty) = (info.name.clone(), info.ty.clone());
            if !seen.insert(name.clone()) {
                continue;
            }
            let children = self.reference(handle);
            out.push(Variable {
                name,
                value,
                ty: (!ty.is_empty() && !session::is_type_variable(&ty)).then_some(ty),
                children,
            });
        }
        out
    }

    pub fn variables(&mut self, reference: u32) -> Vec<Variable> {
        let Some(target) = reference
            .checked_sub(1)
            .and_then(|i| self.refs.get(i as usize))
        else {
            return Vec::new();
        };
        match *target {
            Target::Locals(frame) => self.locals(frame),
            Target::Value(handle) => {
                self.send(json!({ "children": handle }));
                let kids = self
                    .hear(Some(Duration::from_secs(10)))
                    .map(|m| m["children"].clone())
                    .unwrap_or(Json::Null);
                let rows: Vec<(String, String, u64)> = kids
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|k| {
                        (
                            k[0].as_str().unwrap_or("").to_string(),
                            k[1].as_str().unwrap_or("").to_string(),
                            k[2].as_u64().unwrap_or(0),
                        )
                    })
                    .collect();
                rows.into_iter()
                    .map(|(name, value, handle)| Variable {
                        name,
                        value,
                        ty: None,
                        children: self.reference(handle),
                    })
                    .collect()
            }
        }
    }

    /// As Glade's session's: a name in scope in frame `frame`.
    pub fn evaluate(&mut self, frame: usize, expr: &str) -> Option<Variable> {
        let expr = expr.trim();
        self.locals(frame).into_iter().find(|v| v.name == expr)
    }
}

impl Drop for SiloSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The VM's connection, unless it ends before it makes one.
fn accept(listener: &TcpListener, child: &mut Child) -> Result<TcpStream, LaunchError> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match listener.accept() {
            Ok((conn, _)) => {
                conn.set_nonblocking(false).map_err(|e| e.to_string())?;
                return Ok(conn);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!(
                        "Silo's interpreter ended before it connected: {status}"
                    ));
                }
                if Instant::now() > deadline {
                    return Err("Silo's interpreter did not connect".into());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(format!("the debugger's connection failed: {e}")),
        }
    }
}

/// One line, waiting as long as it takes: for the VM's first word.
fn read_line(input: &mut BufReader<TcpStream>) -> Option<Json> {
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => serde_json::from_str(&line).ok(),
    }
}
