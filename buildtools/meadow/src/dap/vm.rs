//! One debugging session on **the VM**: the program built for debugging
//! ([`session::build_reloadable`]), written as an image, and run by
//! `meadow-glade-vm --debug` in a process of its own, which this drives over a
//! connection of its own (`glade/vm/src/interp/debug.rs` says what is said on
//! it). The VM's standard output is the program's, forwarded to the editor
//! as `output` events.
//!
//! What a person sees is worked out here: which source and line each instruction is on, where a step may stop (a
//! *boundary*, the first instruction of a source position), which pcs a
//! breakpoint on a line means, and what each variable is called. The VM says
//! where it is and what its frames hold.
//!
//! **Reloading.** Launched to watch (`"watch": true`), the session builds the
//! program again whenever its package's sources change, as `meadow run
//! --watch` does, and writes the image the VM watches; the VM loads the new
//! build as a segment of its own and says so (`loaded`) before anything calls
//! it. Each segment is worked out here as the first is, from the build that
//! made it: its sources as they were then, its lines, its variables. The
//! tables below are of every segment's pcs and procedures one after another,
//! and a breakpoint is in each segment's code for its line -- the old code's
//! as well as the new, since a call under way runs on in the old.

use super::session::{
    self, Entry, Frame, LaunchError, Mode, Scope, SourceFile, Stop, VarInfo, Variable,
};
use meadow_compiler::core::Loc;
use serde_json::{Value as Json, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A procedure of the VM's: what it is of, and where its code is -- in the
/// pcs of every segment one after another.
struct Proc {
    name: String,
    entry: usize,
    end: usize,
    /// Where the `new` that makes it is, for one a `new` makes.
    origin: Option<usize>,
    /// The segment it is in, and its number there.
    seg: usize,
    local: usize,
}

/// One load's code, as the VM numbers it: where its pcs and procedures begin
/// among every segment's, and the variables of the build that made it.
struct Seg {
    code: usize,
    procs: usize,
    vars: HashMap<u32, VarInfo>,
}

/// A segment's sources are numbered apart from the others': its own numbers,
/// shifted this far.
const SOURCE_SHIFT: u32 = 24;

/// What a session launched to watch rebuilds from, and when.
struct Watch {
    root: std::path::PathBuf,
    path: std::path::PathBuf,
    entry: Option<Entry>,
    image: std::path::PathBuf,
    config: meadow_llvm::Config,
    seen: Vec<(std::path::PathBuf, u64, Option<std::time::SystemTime>)>,
    next: Instant,
}

/// A variable a frame holds, as the VM showed it.
struct Shown {
    var: u32,
    value: String,
    handle: u64,
}

struct Stacked {
    /// The procedure, by its place in the session's table; and the pc, in
    /// every segment's: none for a segment not known.
    proc: Option<usize>,
    pc: usize,
    seg: usize,
    vars: Vec<Shown>,
}

/// What a variables reference stands for, until the program runs again.
enum Target {
    Locals(usize),
    Value(u64),
}

pub struct VmSession {
    child: Child,
    out: TcpStream,
    input: BufReader<TcpStream>,
    procs: Vec<Proc>,
    /// Where each instruction was written.
    locs: Vec<Option<Loc>>,
    pub files: HashMap<u32, SourceFile>,
    segs: Vec<Seg>,
    boundary: Vec<bool>,
    /// Breakpoints, by the file they are in: the lines asked for, and every
    /// pc the VM is told of.
    breakpoints: HashMap<std::path::PathBuf, (Vec<u32>, Vec<usize>)>,
    /// What has arrived of a line the VM is still saying.
    pending: String,
    /// What the VM said while the session waited for something else.
    heard: VecDeque<Json>,
    /// The sources and variables of the last build written, for the segment
    /// the VM makes of it.
    built: Option<(HashMap<u32, SourceFile>, HashMap<u32, VarInfo>)>,
    /// How many reloads the VM has said the end of.
    reloads: usize,
    watch: Option<Watch>,
    output: Arc<Mutex<String>>,
    /// Whether the program's output has ended: it has, when it has exited.
    output_ended: Arc<AtomicBool>,
    /// Whether the VM has been told to begin.
    begun: bool,
    finished: Option<Stop>,
    stack: Vec<Stacked>,
    refs: Vec<Target>,
}

impl VmSession {
    /// Build `path` for debugging, starting at `entry` if there is one, and
    /// stand it at its start on the VM, not yet run.
    pub fn launch_at(path: &Path, entry: Option<&Entry>) -> Result<VmSession, LaunchError> {
        VmSession::launch_with(path, entry, None)
    }

    /// [`VmSession::launch_at`], its frames kept as `stack` says: on the
    /// heap, or on a native stack that grows -- which is
    /// what it is unless the launch or `MEADOW_STACK` says `heap`. A fixed
    /// stack is never what a debugger wants.
    pub fn launch_with(
        path: &Path,
        entry: Option<&Entry>,
        stack: Option<meadow_llvm::Stack>,
    ) -> Result<VmSession, LaunchError> {
        VmSession::launch_watching(path, entry, stack, false)
    }

    /// [`VmSession::launch_with`], and, if `watch`, built again and
    /// reloaded whenever the package's sources change: see [`Self::tick`].
    pub fn launch_watching(
        path: &Path,
        entry: Option<&Entry>,
        stack: Option<meadow_llvm::Stack>,
        watch: bool,
    ) -> Result<VmSession, LaunchError> {
        let began = Instant::now();
        let say = |what: &str| {
            if std::env::var_os("MEADOW_DAP_TIMES").is_some() {
                eprintln!("dap: {what} at {:?}", began.elapsed());
            }
        };
        let (program, packages) = session::build_reloadable(path, entry, crate::aot::reloadable())?;
        say("built");
        let origins: Vec<(u32, u32)> = program.origins.iter().map(|(c, o)| (c.0, o.0)).collect();
        let (files, vars) = session::sources(&packages, origins);
        let dir = std::env::temp_dir()
            .join("meadow-glade-debug")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // One image a session: a process may hold several, as tests do.
        static SESSIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let image = dir.join(format!(
            "debugged-{}.image",
            SESSIONS.fetch_add(1, Ordering::Relaxed)
        ));
        // A debugger's program counts as the build would, and its stack
        // grows: a deep recursion is something to look at, not a
        // reason to stop.
        let asked = stack.or_else(|| {
            std::env::var("MEADOW_STACK")
                .ok()
                .and_then(|s| meadow_llvm::Stack::named(&s))
        });
        let config = meadow_llvm::Config {
            stack: match asked {
                Some(meadow_llvm::Stack::Heap) => meadow_llvm::Stack::Heap,
                _ => meadow_llvm::Stack::Growable,
            },
            ..crate::aot::config()
        };
        crate::aot::write_image_with(&image, program, Vec::new(), config)?;
        say("image written");
        let watch = watch.then(|| {
            let full = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let root = if full.is_dir() {
                full.clone()
            } else {
                full.parent().map_or(full.clone(), Path::to_path_buf)
            };
            Watch {
                seen: crate::watch::sources(&root),
                root,
                path: full,
                entry: entry.cloned(),
                image: image.clone(),
                config,
                next: Instant::now(),
            }
        });

        let listener =
            TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("cannot listen: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let mut vm = Command::new(crate::aot::interpreter()?);
        if watch.is_some() {
            vm.arg("--watch");
        }
        let mut child = vm
            .arg("--debug")
            .arg(port.to_string())
            .arg(&image)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start the VM: {e}"))?;
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
        let ready = read_line(&mut input).ok_or("the VM said nothing")?;
        let ready = ready["ready"].clone();
        say("ready");
        let mut s = VmSession {
            child,
            out: conn,
            input,
            procs: Vec::new(),
            boundary: Vec::new(),
            locs: Vec::new(),
            files: HashMap::new(),
            segs: Vec::new(),
            breakpoints: HashMap::new(),
            pending: String::new(),
            heard: VecDeque::new(),
            built: None,
            reloads: 0,
            watch,
            output,
            output_ended,
            begun: false,
            finished: None,
            stack: Vec::new(),
            refs: Vec::new(),
        };
        s.absorb(&ready, files, vars);
        say("set up");
        Ok(s)
    }

    /// Take in the VM's description of its next segment, made from a build
    /// whose sources and variables are `files` and `vars`, and tell the VM
    /// where its lines and breakpoints are.
    fn absorb(
        &mut self,
        desc: &Json,
        files: HashMap<u32, SourceFile>,
        vars: HashMap<u32, VarInfo>,
    ) {
        let seg = self.segs.len();
        let (code, procs) = (self.locs.len(), self.procs.len());
        let shift = |source: u32| ((seg as u32) << SOURCE_SHIFT) | source;
        let n = desc["code"].as_u64().unwrap_or(0) as usize;
        for (local, p) in desc["procs"].as_array().into_iter().flatten().enumerate() {
            self.procs.push(Proc {
                name: p["name"].as_str().unwrap_or("?").to_string(),
                entry: code + p["entry"].as_u64().unwrap_or(0) as usize,
                end: code + p["end"].as_u64().unwrap_or(0) as usize,
                origin: p["origin"].as_u64().map(|o| code + o as usize),
                seg,
                local,
            });
        }
        self.locs.resize(code + n, None);
        for l in desc["locs"].as_array().into_iter().flatten() {
            let (Some(pc), Some(source), Some(start), Some(end)) =
                (l[0].as_u64(), l[1].as_u64(), l[2].as_u64(), l[3].as_u64())
            else {
                continue;
            };
            if let Some(slot) = self
                .locs
                .get_mut(code + pc as usize)
                .filter(|_| (pc as usize) < n)
            {
                *slot = Some(Loc {
                    source: shift(source as u32),
                    span: meadow_compiler::span::Span::new(start as u32, end as u32),
                });
            }
        }
        for (id, f) in files {
            self.files.insert(shift(id), f);
        }
        self.segs.push(Seg { code, procs, vars });
        self.boundary = self.boundaries();
        // Its breakpoints before its setup, which is what the VM waits for
        // before anything runs its code.
        if seg > 0 {
            let asked: Vec<(std::path::PathBuf, Vec<u32>)> = self
                .breakpoints
                .iter()
                .map(|(path, (lines, _))| (path.clone(), lines.clone()))
                .collect();
            for (path, lines) in asked {
                self.set_breakpoints(&path, &lines);
            }
        }
        let keys: Vec<Json> = (code..code + n)
            .filter_map(|pc| self.key(pc).map(|k| json!([pc - code, k])))
            .collect();
        let boundary: Vec<usize> = (code..code + n)
            .filter(|pc| self.boundary[*pc])
            .map(|pc| pc - code)
            .collect();
        self.send(json!({ "setup": { "segment": seg, "boundary": boundary, "keys": keys } }));
    }

    /// The VM's description of a segment a reload added: of the build last
    /// written, or -- if this session wrote none -- of sources as the first
    /// build's were.
    fn loaded(&mut self, m: &Json) {
        let seg = m["segment"].as_u64().unwrap_or(0) as usize;
        if seg != self.segs.len() {
            return;
        }
        let (files, vars) = self.built.take().unwrap_or_else(|| {
            let first = self
                .files
                .iter()
                .filter(|(id, _)| **id >> SOURCE_SHIFT == 0)
                .map(|(id, f)| (*id, f.clone()))
                .collect();
            (first, self.segs[0].vars.clone())
        });
        self.absorb(&m["description"], files, vars);
    }

    /// The segment and pc, as the VM numbers them, of pc `pc` of every
    /// segment's.
    fn vm_pc(&self, pc: usize) -> (usize, usize) {
        let seg = self
            .segs
            .partition_point(|s| s.code <= pc)
            .saturating_sub(1);
        (seg, pc - self.segs.get(seg).map_or(0, |s| s.code))
    }

    /// Something said, for the editor to show as the program's output is.
    fn say(&self, text: String) {
        self.output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_str(&text);
    }

    /// Under `"watch"`, look at the package's sources, if it is time to: if
    /// they changed, build the program again, write the image the VM watches,
    /// and wait for the VM to say how its reload went.
    pub fn tick(&mut self) {
        let Some(w) = &mut self.watch else { return };
        if Instant::now() < w.next || self.finished.is_some() {
            return;
        }
        w.next = Instant::now() + Duration::from_millis(250);
        let now = crate::watch::sources(&w.root);
        if now == w.seen {
            return;
        }
        w.seen = now;
        let (path, entry, image, config) =
            (w.path.clone(), w.entry.clone(), w.image.clone(), w.config);
        self.say("Rebuilding: the sources changed\n".into());
        let built = session::build_reloadable(&path, entry.as_ref(), crate::aot::reloadable());
        let written = built.and_then(|(program, packages)| {
            let origins: Vec<(u32, u32)> =
                program.origins.iter().map(|(c, o)| (c.0, o.0)).collect();
            let sources = session::sources(&packages, origins);
            crate::aot::write_image_with(&image, program, Vec::new(), config)?;
            Ok(sources)
        });
        match written {
            Err(e) => self.say(format!("not rebuilt: {e}\n")),
            Ok(sources) => {
                self.built = Some(sources);
                let before = self.reloads;
                let deadline = Instant::now() + Duration::from_secs(60);
                while self.reloads == before && Instant::now() < deadline {
                    match self.listen(Some(Duration::from_millis(100))) {
                        Some(m) => self.heard.push_back(m),
                        None if self.child.try_wait().ok().flatten().is_some() => break,
                        None => {}
                    }
                }
            }
        }
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

    /// The next thing the VM says, waiting at most `wait`: what it said
    /// while the session waited for a reload first.
    fn hear(&mut self, wait: Option<Duration>) -> Option<Json> {
        match self.heard.pop_front() {
            Some(m) => Some(m),
            None => self.listen(wait),
        }
    }

    /// The next thing the VM says on its connection, waiting at most `wait`,
    /// but for what it says of a reload, which is taken in at once. A line
    /// that is only partly there when the wait ends is kept for the next
    /// look.
    fn listen(&mut self, wait: Option<Duration>) -> Option<Json> {
        let _ = self.input.get_ref().set_read_timeout(wait);
        loop {
            match self.input.read_line(&mut self.pending) {
                Ok(0) => return None,
                Ok(_) if self.pending.ends_with('\n') => {
                    let line = std::mem::take(&mut self.pending);
                    let Ok(msg) = serde_json::from_str::<Json>(&line) else {
                        continue;
                    };
                    if let Some(m) = msg.get("loaded") {
                        self.loaded(&m.clone());
                    } else if msg.get("reloaded").is_some() || msg.get("refused").is_some() {
                        self.reloads += 1;
                    } else {
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

    /// Whether it was launched to watch its sources.
    pub fn watching(&self) -> bool {
        self.watch.is_some()
    }

    pub fn file(&self, loc: Loc) -> Option<&SourceFile> {
        self.files.get(&loc.source)
    }

    // --- breakpoints ------------------------------------------------------

    /// The first boundary of each line in each procedure, on the line asked
    /// for or a little after it.
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
        // Each line's pcs in each segment, whichever build of the file each
        // was made from.
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
        self.breakpoints.insert(wanted, (lines.to_vec(), here));
        let all: Vec<Json> = self
            .breakpoints
            .values()
            .flat_map(|(_, pcs)| pcs)
            .map(|pc| {
                let (seg, pc) = self.vm_pc(*pc);
                json!([seg, pc])
            })
            .collect();
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
                let procs: Vec<Json> = self
                    .procs_of(&name, source)
                    .into_iter()
                    .map(|i| json!([self.procs[i].seg, self.procs[i].local]))
                    .collect();
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

    /// How it stopped, or `None` while it is still going. The budget is not
    /// instructions but how long to wait for news.
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
            let segs = &self.segs;
            self.stack = frames
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    let seg = f["segment"].as_u64().unwrap_or(0) as usize;
                    let known = segs.get(seg);
                    (f, seg, known)
                })
                .map(|(f, seg, known)| Stacked {
                    proc: known.map(|s| s.procs + f["proc"].as_u64().unwrap_or(0) as usize),
                    pc: known.map_or(0, |s| s.code) + f["pc"].as_u64().unwrap_or(0) as usize,
                    seg,
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
        let rows: Vec<Option<usize>> = self.stack.iter().map(|f| f.proc).collect();
        rows.iter()
            .enumerate()
            .filter(|(_, p)| {
                !p.and_then(|p| procs.get(p))
                    .is_some_and(|p| p.name.starts_with('.'))
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn frames(&mut self, max: usize) -> Vec<Frame> {
        self.stacked();
        let shown = self.shown_frames();
        let rows: Vec<(Option<usize>, usize, usize)> = shown
            .iter()
            .map(|i| (self.stack[*i].proc, self.stack[*i].pc, self.stack[*i].seg))
            .collect();
        rows.into_iter()
            .take(max)
            .map(|(proc, pc, seg)| {
                let p = proc.and_then(|p| self.procs.get(p));
                // The definition's own name: not
                // its module's, nor which of its methods or frames this is.
                let name = session::display_name(p.map_or("?", |p| {
                    let def = p.name.split('/').next().unwrap_or("?");
                    def.rsplit('.').next().unwrap_or(def)
                }));
                Frame {
                    // And the reload its code came in, if not the first load.
                    name: match seg {
                        0 => name,
                        n => format!("{name} [reload {n}]"),
                    },
                    // A frame waiting on the heap is where the call that
                    // made its continuation is.
                    loc: match p {
                        Some(p) if pc == p.entry && p.origin.is_some() => {
                            p.origin.and_then(|o| self.loc_near(o))
                        }
                        Some(_) => self.loc_near(pc),
                        None => None,
                    },
                }
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

    /// The named variables of frame `frame`:
    /// those a person wrote, under the names they wrote.
    fn locals(&mut self, frame: usize) -> Vec<Variable> {
        self.stacked();
        // Frame `frame` of those shown, not of all the VM has.
        let Some(frame) = self.shown_frames().get(frame).copied() else {
            return Vec::new();
        };
        let (seg, rows): (usize, Vec<(u32, String, u64)>) = self
            .stack
            .get(frame)
            .map(|f| {
                let rows = f
                    .vars
                    .iter()
                    .map(|v| (v.var, v.value.clone(), v.handle))
                    .collect();
                (f.seg, rows)
            })
            .unwrap_or_default();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (var, value, handle) in rows {
            // Named as the build its segment was made from names them.
            let Some(info) = self.segs.get(seg).and_then(|s| s.vars.get(&var)) else {
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

    /// A name in scope in frame `frame`.
    pub fn evaluate(&mut self, frame: usize, expr: &str) -> Option<Variable> {
        let expr = expr.trim();
        self.locals(frame).into_iter().find(|v| v.name == expr)
    }
}

impl Drop for VmSession {
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
                    return Err(format!("the VM ended before it connected: {status}"));
                }
                if Instant::now() > deadline {
                    return Err("the VM did not connect".into());
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
