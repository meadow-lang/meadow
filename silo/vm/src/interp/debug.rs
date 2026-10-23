//! **The VM under a debugger**: what `meadow dap` drives when it debugs a
//! program on Silo's runtime (`docs/RUNTIME-MERGE.md`, "The VM").
//!
//! `meadow-silo-vm --debug PORT IMAGE` connects to the adapter on `PORT` and
//! speaks a small protocol of its own over that connection -- one JSON
//! object a line -- leaving its standard output to the program, which the
//! adapter forwards to the editor. The adapter does what Glade's session
//! does with source files, names and lines; the VM says where it is, what
//! its frames hold, and stops where it is told to.
//!
//! **Frames.** Under a debugger each activation of the interpreter's loop
//! keeps an [`Act`] on the native stack, linked to its caller's, with the
//! chain's head kept by the runtime per segment (`meadow_silo::frames`). A
//! stack trace walks it; a step compares depths, which a tail call keeps as
//! it keeps the frame.
//!
//! **Stopping.** Before each instruction -- and so never twice at one, since
//! the instruction a stop was at runs as soon as the program does -- at a
//! breakpoint's pc; at a
//! *boundary* -- the first instruction of a source position, which the
//! adapter works out -- when the step running says so, as Glade's session
//! decides it; or when the adapter asked for a pause. Then the program waits,
//! answering questions, until it is told to run again.
//!
//! **What the VM is told** (adapter to VM): `setup` (the boundaries, and a
//! key per pc saying which line it is on), `breakpoints`, `run` (how far),
//! `stack`, `children` (of a value shown before), `pause`, `terminate`.
//! **What it says**: `ready` (its procedures and where each instruction was
//! written), `stopped`, `exception`, `exited`, and the answers.

use super::{Env, V, Vm, d, vm};
use meadow_rt::desc;
use meadow_silo::heap::{self, Word};
use serde_json::{Value as Json, json};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};

/// An activation's record, on the native stack of the loop it belongs to.
pub(super) struct Act {
    pub(super) parent: Word,
    pub(super) depth: u32,
    pub(super) proc: u32,
    pub(super) pc: usize,
    pub(super) env: *const Env,
}

impl Act {
    /// How deep the activation `top` records is: `0` for none.
    pub(super) fn depth_of(top: Word) -> u32 {
        Act::at(top).map_or(0, |a| a.depth)
    }

    /// The record `top` names, if any: the head of the running chain, or a
    /// parent of one, which lives as long as the activation it records.
    fn at(top: Word) -> Option<&'static Act> {
        // Safety: every record on the chain is the frame of a loop still
        // running further up the native stack.
        (top != 0).then(|| unsafe { &*(top as *const Act) })
    }
}

/// How far to run.
enum Mode {
    Continue,
    In,
    Over,
    Out,
    /// Until one of these procedures is entered: a definition's, and what is
    /// made inside it.
    Enter(HashSet<u32>),
}

struct State {
    mode: Mode,
    /// The key the running step began on, and the depth.
    from: u64,
    depth: u32,
    /// Values shown since the last stop, for `children` to take apart.
    handles: Vec<(Word, i64)>,
}

pub(super) struct Debugger {
    /// Where a step may stop, and which line each pc is on: told once,
    /// before the program begins, and only read after.
    boundary: OnceLock<Vec<bool>>,
    keys: OnceLock<Vec<u64>>,
    out: Mutex<TcpStream>,
    inbox: Mutex<mpsc::Receiver<Json>>,
    breakpoint: Vec<AtomicBool>,
    pause: AtomicBool,
    state: Mutex<State>,
}

static DEBUGGER: OnceLock<Debugger> = OnceLock::new();

pub(super) fn debugger() -> Option<&'static Debugger> {
    DEBUGGER.get()
}

/// Connect to the adapter on `port`, tell it what the loaded program is, and
/// wait to be told how to begin.
pub fn start(port: u16) -> Result<(), String> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("could not reach the debugger on port {port}: {e}"))?;
    let reader = stream.try_clone().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();
    let v = vm();
    let n = v.code.ins.len();
    let made = Debugger {
        boundary: OnceLock::new(),
        keys: OnceLock::new(),
        out: Mutex::new(stream),
        inbox: Mutex::new(rx),
        breakpoint: (0..n).map(|_| AtomicBool::new(false)).collect(),
        pause: AtomicBool::new(false),
        state: Mutex::new(State {
            mode: Mode::Continue,
            from: 0,
            depth: 0,
            handles: Vec::new(),
        }),
    };
    if DEBUGGER.set(made).is_err() {
        return Err("a debugger was started twice".into());
    }
    meadow_silo::on_failure(failed);
    // What arrives while the program runs is acted on at once if it can be
    // -- a pause, new breakpoints, the end -- and kept for the next stop if
    // not.
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            let Ok(msg) = serde_json::from_str::<Json>(&line) else {
                continue;
            };
            let me = debugger().expect("started");
            if msg.get("pause").is_some() {
                me.pause.store(true, Ordering::Relaxed);
            } else if let Some(pcs) = msg.get("breakpoints") {
                me.set_breakpoints(pcs);
            } else if msg.get("terminate").is_some() {
                std::process::exit(0);
            } else if tx.send(msg).is_err() {
                break;
            }
        }
        // The adapter is gone: so is the program.
        std::process::exit(0);
    });
    let me = debugger().expect("set above");
    me.send(json!({ "ready": v.description() }));
    // The setup, and then how to begin.
    loop {
        let msg = me.next();
        if let Some(setup) = msg.get("setup") {
            let mut boundary = vec![false; n];
            for b in setup["boundary"].as_array().into_iter().flatten() {
                if let Some(pc) = b.as_u64().filter(|pc| (*pc as usize) < n) {
                    boundary[pc as usize] = true;
                }
            }
            let mut keys = vec![0; n];
            for pair in setup["keys"].as_array().into_iter().flatten() {
                if let (Some(pc), Some(k)) = (pair[0].as_u64(), pair[1].as_u64())
                    && (pc as usize) < n
                {
                    keys[pc as usize] = k;
                }
            }
            let _ = me.boundary.set(boundary);
            let _ = me.keys.set(keys);
        } else if let Some(run) = msg.get("run") {
            let mut st = me.state();
            st.mode = mode(run);
            st.from = 0;
            st.depth = 0;
            return Ok(());
        }
    }
}

fn mode(run: &Json) -> Mode {
    match run["mode"].as_str().unwrap_or("continue") {
        "in" => Mode::In,
        "over" => Mode::Over,
        "out" => Mode::Out,
        "enter" => Mode::Enter(
            run["procs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_u64().map(|p| p as u32))
                .collect(),
        ),
        _ => Mode::Continue,
    }
}

impl Debugger {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn send(&self, msg: Json) {
        // One write a message: formatted straight onto the socket it would
        // be a write for every piece of it, and a program's description is
        // a great many pieces.
        let mut text = msg.to_string();
        text.push('\n');
        let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    /// The next command kept for a stop; the end of the program if the
    /// adapter has gone.
    fn next(&self) -> Json {
        match self.inbox.lock().unwrap_or_else(|p| p.into_inner()).recv() {
            Ok(m) => m,
            Err(_) => std::process::exit(0),
        }
    }

    /// Which line `pc` is on, as the adapter numbered lines.
    fn key(&self, pc: usize) -> u64 {
        self.keys.get().map_or(0, |k| k[pc])
    }

    fn set_breakpoints(&self, pcs: &Json) {
        for b in &self.breakpoint {
            b.store(false, Ordering::Relaxed);
        }
        for pc in pcs.as_array().into_iter().flatten() {
            if let Some(b) = pc.as_u64().and_then(|pc| self.breakpoint.get(pc as usize)) {
                b.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Before the instruction `act` is at: stop there if anything says to.
    pub(super) fn at(&self, act: &Act) {
        let pc = act.pc;
        let boundary = self.boundary.get().is_some_and(|b| b[pc]);
        let reason = if self.pause.swap(false, Ordering::Relaxed) {
            Some("pause")
        } else if self.breakpoint[pc].load(Ordering::Relaxed) {
            Some("breakpoint")
        } else if !boundary {
            None
        } else {
            let st = self.state();
            {
                // By line, as a person reads code -- and a call begun on the
                // line it was made from is somewhere new all the same, so a
                // change of frame counts as moving too.
                let moved = self.key(pc) != st.from;
                let (here, from) = (act.depth, st.depth);
                let stop = match &st.mode {
                    Mode::Continue => false,
                    Mode::Enter(procs) => procs.contains(&act.proc),
                    Mode::In => moved || here != from,
                    Mode::Over => (here == from && moved) || here < from,
                    Mode::Out => here < from,
                };
                stop.then_some(match st.mode {
                    Mode::Enter(_) => "entry",
                    _ => "step",
                })
            }
        };
        if let Some(reason) = reason {
            self.send(json!({ "stopped": reason, "pc": pc }));
            self.serve(Some(act));
        }
    }

    /// Answer questions while stopped at `act`, until told to run.
    fn serve(&self, act: Option<&Act>) {
        self.state().handles.clear();
        loop {
            let msg = self.next();
            if let Some(max) = msg.get("stack") {
                let max = max.as_u64().unwrap_or(1000) as usize;
                let frames = self.stack(max);
                self.send(json!({ "stack": frames }));
            } else if let Some(h) = msg.get("children") {
                let kids = self.children(h.as_u64().unwrap_or(0) as usize);
                self.send(json!({ "children": kids }));
            } else if let Some(run) = msg.get("run") {
                let mut st = self.state();
                st.mode = mode(run);
                if let Some(a) = act {
                    st.from = self.key(a.pc);
                    st.depth = a.depth;
                }
                st.handles.clear();
                return;
            }
        }
    }

    /// The running chain, innermost first, at most `max` deep: each frame's
    /// procedure, pc, and the named registers that hold something.
    fn stack(&self, max: usize) -> Vec<Json> {
        let v = vm();
        let mut out = Vec::new();
        let mut at = Act::at(meadow_silo::frames::top());
        while let Some(a) = at {
            if out.len() >= max {
                break;
            }
            let p = &v.procs[a.proc as usize];
            // Safety: the activation's registers, which nothing writes while
            // the program is stopped.
            let env: &Env = unsafe { &*a.env };
            let mut vars = Vec::new();
            for (slot, var) in p.slot_vars.iter().enumerate() {
                let (Some(var), Some(V::Val(w))) = (var, env.get(slot)) else {
                    continue;
                };
                let dd = d(p.slot_descs[slot], env);
                let (shown, handle) = self.value(*w, dd);
                vars.push(json!([var, shown, handle]));
            }
            out.push(json!({ "proc": a.proc, "pc": a.pc, "vars": vars }));
            at = Act::at(a.parent);
        }
        out
    }

    /// `w`, described by `d`, as a debugger shows it, and the handle its
    /// parts are asked for by -- `0` when it has none.
    fn value(&self, w: Word, d: i64) -> (String, usize) {
        let known = (0..16).contains(&d) && d != desc::ANY;
        if !known {
            return (format!("{w:#x}"), 0);
        }
        let mut shown = meadow_silo::show::show(w, d);
        if shown.chars().count() > 200 {
            shown = shown.chars().take(200).collect::<String>() + "…";
        }
        let parts = d == desc::REF
            && heap::is_block(w)
            && matches!(
                heap::kind(w),
                heap::DATA
                    | heap::RECORD
                    | heap::ARRAY
                    | heap::MUT_ARRAY
                    | heap::CLOSURE
                    | heap::CELL
            )
            && heap::len(w) > 0;
        if !parts {
            return (shown, 0);
        }
        let mut st = self.state();
        st.handles.push((w, d));
        (shown, st.handles.len())
    }

    /// The parts of the value handle `h` names: a constructor's fields by
    /// name, a record's labels, an array's elements, a closure's captures.
    fn children(&self, h: usize) -> Vec<Json> {
        let Some((w, _)) = h
            .checked_sub(1)
            .and_then(|i| self.state().handles.get(i).copied())
        else {
            return Vec::new();
        };
        let names = meadow_silo::show::names();
        let n = heap::len(w);
        let field = |i: usize| (heap::field(w, i), heap::field_desc(w, i));
        let mut out = Vec::new();
        let mut row = |name: String, (x, dx): (Word, i64)| {
            let (shown, handle) = self.value(x, dx);
            out.push(json!([name, shown, handle]));
        };
        match heap::kind(w) {
            heap::DATA => {
                let labels = names
                    .fields
                    .get(heap::meta(w) as usize)
                    .and_then(|f| f.as_ref());
                for i in 0..n {
                    let name = labels
                        .and_then(|l| l.get(i).cloned())
                        .unwrap_or_else(|| i.to_string());
                    row(name, field(i));
                }
            }
            heap::RECORD => {
                for i in (0..n.saturating_sub(1)).step_by(2) {
                    let label = names
                        .syms
                        .get(heap::field(w, i) as usize)
                        .cloned()
                        .unwrap_or_else(|| format!("#{i}"));
                    row(label, field(i + 1));
                }
            }
            heap::ARRAY | heap::MUT_ARRAY => {
                for i in 0..n.min(500) {
                    row(format!("[{i}]"), field(i));
                }
            }
            heap::CLOSURE => {
                for i in 0..n {
                    row(format!("capture {i}"), field(i));
                }
            }
            heap::CELL => row("value".into(), field(0)),
            _ => {}
        }
        out
    }
}

/// What the runtime calls first when the program fails: say so, and stay to
/// be looked at, until the adapter lets the program end.
fn failed(msg: &str) {
    let Some(me) = debugger() else { return };
    me.send(json!({ "exception": msg }));
    let at = Act::at(meadow_silo::frames::top());
    me.serve(at);
}

/// The program finished with `shown`: tell the adapter, before the runtime
/// ends the process.
pub fn exited(shown: Option<String>) {
    if let Some(me) = debugger() {
        let _ = std::io::stdout().flush();
        me.send(json!({ "exited": { "ok": shown } }));
    }
}

impl Vm {
    /// What the adapter needs to know of the loaded program: each procedure's
    /// name and code, and where each instruction was written.
    fn description(&self) -> Json {
        let mut order: Vec<usize> = (0..self.procs.len()).collect();
        order.sort_by_key(|i| self.procs[*i].entry);
        let mut procs = vec![Json::Null; self.procs.len()];
        for (k, i) in order.iter().enumerate() {
            let p = &self.procs[*i];
            let end = order
                .get(k + 1)
                .map_or(self.code.ins.len(), |j| self.procs[*j].entry as usize);
            procs[*i] = json!({ "name": p.name, "entry": p.entry, "end": end });
        }
        let locs: Vec<Json> = self
            .code
            .locs
            .iter()
            .enumerate()
            .filter_map(|(pc, l)| l.map(|l| json!([pc, l.source, l.span.start, l.span.end])))
            .collect();
        json!({ "procs": procs, "locs": locs, "code": self.code.ins.len() })
    }
}
