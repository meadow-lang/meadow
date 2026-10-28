//! One debugging session: a program built for debugging, running on the VM,
//! and everything a debugger asks of it -- where it is, what the names hold, and
//! how far to run before stopping again.
//!
//! Nothing here speaks the protocol. [`super`] turns requests into calls on a
//! [`Session`] and its answers back into messages, which keeps this testable
//! with ordinary Rust and keeps the protocol code dull.
//!
//! # A call stack for a machine without one
//!
//! The VM has no `call` or `ret`: returning from a function is invoking the
//! continuation it was given, and that continuation is a frame on the frame
//! stack, or now and then a heap closure, laid out alike. Either way it is
//! a stack in every way that matters to a person reading one -- the
//! continuation captures what the caller will need when it resumes, including
//! *its* continuation -- so [`Session::frames`] walks that chain. The compiler
//! records which names are return continuations
//! ([`meadow_bytecode::DebugInfo::returns`]) and which name each register and
//! each capture holds, and that is enough to draw the stack a source-level
//! debugger would.
//!
//! # Where a step stops
//!
//! At a *boundary*: the first instruction of a source position (a call, a
//! branch, a body -- see `meadow_core::Term::Loc`). Stepping over and out
//! compare return continuations rather than counting frames: the frame a step
//! began in is identified by the continuation it will answer, which the VM keeps
//! current across collections ([`meadow_glade::Vm::pinned`]). Counting frames
//! would mean walking the whole chain at every boundary, which a deep recursion
//! makes quadratic.

use crate::pipeline;
use crate::profile::{Profile, Resolved};
use meadow_bytecode::{DebugInfo, NameDesc, Pc, Program, Reg};
use meadow_compiler::core::Loc;
use meadow_compiler::hir;
use meadow_compiler::infer::Renderer;
use meadow_compiler::intern::InternedString;
use meadow_compiler::source::SourceKind;
use meadow_glade::{Kind, Value, Vm};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A source file the program was built from.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Where it is on disk, if it is anywhere: a `Std` module is, once the
    /// editor support has extracted the library.
    pub path: Option<PathBuf>,
    /// The name it was compiled under -- a path, or `Std/Maybe.mw`.
    pub name: String,
    content: InternedString,
    /// Byte offset of each line's start.
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub(super) fn new(path: Option<PathBuf>, name: String, content: InternedString) -> SourceFile {
        let mut line_starts = vec![0];
        for (i, b) in content.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceFile {
            path,
            name,
            content,
            line_starts,
        }
    }

    /// 1-based line and column of a byte offset. Columns count characters,
    /// which is what an editor shows for everything but astral-plane text.
    pub fn position(&self, offset: u32) -> (u32, u32) {
        let line = self.line_starts.partition_point(|&s| s <= offset).max(1) - 1;
        let start = self.line_starts[line] as usize;
        let end = (offset as usize).min(self.content.len());
        let col = self
            .content
            .get(start..end)
            .map_or(0, |s| s.chars().count());
        (line as u32 + 1, col as u32 + 1)
    }

    pub(super) fn line_of(&self, offset: u32) -> u32 {
        self.position(offset).0
    }
}

/// A name a person wrote, as a register or a capture holds it.
#[derive(Debug, Clone)]
pub(super) struct VarInfo {
    pub(super) name: String,
    pub(super) ty: String,
}

/// Why the program is not running.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    /// Paused before its first statement, because the launch asked for that.
    Entry,
    Breakpoint,
    /// A step finished.
    Step,
    /// Someone asked it to stop.
    Pause,
    /// It failed. It can be inspected, but not resumed.
    Exception(String),
    /// It finished, with the value `main` produced or the error it stopped on.
    Exited(Result<String, String>),
}

/// How to run until the next stop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// Until a breakpoint.
    Continue,
    /// To the next boundary anywhere.
    StepIn,
    /// To the next boundary in this function or the one it returns to.
    StepOver,
    /// To the next boundary in the function this one returns to.
    StepOut,
    /// To the first boundary inside the top-level definition of this name, in
    /// the source with this id.
    Enter(InternedString, u32),
}

/// One frame of the reconstructed call stack.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The definition it is part of.
    pub name: String,
    /// Where it is -- for a pending frame, where it will resume.
    pub loc: Option<Loc>,
    /// The names it can see, newest first, with their values: Glade's; the
    /// Silo session keeps its own.
    pub(super) vars: Vec<(u32, Value)>,
}

/// A row in a variables view.
#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub ty: Option<String>,
    /// Non-zero when it can be expanded: pass it to [`Session::variables`].
    pub children: u32,
}

/// What a variables reference stands for, for as long as the program stays
/// stopped. Everything is renumbered when it runs again, which is the
/// protocol's rule too.
#[derive(Debug, Clone, Copy)]
enum Target {
    Locals(usize),
    Registers,
    Handlers,
    Heap,
    Value(Value),
}

/// The scopes a frame offers.
#[derive(Debug, Clone)]
pub struct Scope {
    pub name: &'static str,
    pub reference: u32,
    /// Something to leave collapsed until asked.
    pub expensive: bool,
}

pub struct Session {
    image: &'static Program,
    debug: &'static DebugInfo,
    vm: Vm<'static>,
    pub files: HashMap<u32, SourceFile>,
    vars: HashMap<u32, VarInfo>,
    /// `boundary[pc]`: the first instruction of a source position.
    boundary: Vec<bool>,
    /// `breakpoint[pc]`: stop before running it.
    breakpoint: Vec<bool>,
    /// What `print` and `println` have written since the last look.
    output: Arc<Mutex<String>>,
    mode: Mode,
    /// Where the running step began, for [`Mode::StepIn`] and [`Mode::StepOver`].
    step_from: Option<(u32, u32)>,
    /// Stop checks are skipped for the one instruction a resume starts on:
    /// it is where the last stop was, and stopping there again would never
    /// move.
    resumed: bool,
    /// Whether the program has stopped at all yet. Before it has, the first
    /// instruction is not somewhere it already stopped, and a breakpoint on it
    /// has to count.
    has_stopped: bool,
    /// Set by a failure or by the end: nothing more will run.
    finished: Option<Stop>,
    /// The instruction a failure happened at.
    failed_at: Option<Pc>,
    refs: Vec<Target>,
    pub entry: Pc,
}

/// Why a launch could not start.
pub type LaunchError = String;

/// What to run instead of `main`: an expression, in the scope of a module.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The module the expression is written in -- usually the one defining the
    /// function it calls.
    pub module: PathBuf,
    /// A Meadow expression: `eval [;] (Int 1)`.
    pub expression: String,
}

/// The name the entry definition gets. It cannot be a name anyone writes by
/// accident, and it has to be one the lexer accepts.
pub(super) const ENTRY: &str = "debug'entry'";

impl Session {
    /// Build `path` for debugging and stand it at its entry point, not yet run.
    pub fn launch(path: &Path) -> Result<Session, LaunchError> {
        Session::launch_at(path, None)
    }

    /// [`Session::launch`], starting at `entry` rather than at `main`.
    pub fn launch_at(path: &Path, entry: Option<&Entry>) -> Result<Session, LaunchError> {
        let (program, packages) = build(path, entry)?;
        let image = meadow_codegen::compile_with_debug_info(&program).map_err(|e| e.msg)?;
        Session::new(image, &packages)
    }

    /// A session over an image that was compiled with debug information, with
    /// `packages` saying where its sources are and what its names are.
    pub fn new(
        image: Program,
        packages: &[meadow_compiler::CompiledPackage],
    ) -> Result<Session, LaunchError> {
        // One program per session, and a session per process in practice: the
        // machine borrows its image for as long as it runs, and leaking one
        // image per launch is simpler than a self-referential struct.
        let image: &'static Program = Box::leak(Box::new(image));
        let debug: &'static DebugInfo = image
            .debug
            .as_deref()
            .ok_or("the program was compiled without debug information")?;
        let entry = image.entry.ok_or("the program has no entry point")?;

        let (files, vars) = sources(packages, debug.origins.iter().map(|(c, o)| (*c, *o)));

        let n = image.code.len();
        let mut boundary = vec![false; n];
        for (pc, slot) in boundary.iter_mut().enumerate() {
            let here = debug.loc(pc as Pc);
            if here.is_none() {
                continue;
            }
            // A continuation starts out in the position of the call that made
            // it. That is the right thing to show for it, and the wrong place
            // to stop: the call was already stopped at on the way in.
            if let Some(r) = debug.region_at(pc as Pc) {
                *slot = r.origin != here;
                continue;
            }
            *slot = pc == 0 || debug.loc(pc as Pc - 1) != here;
        }

        let output = Arc::new(Mutex::new(String::new()));
        let mut vm = Vm::new(image);
        let sink = output.clone();
        vm.io.output = Some(Box::new(move |s: &str| {
            sink.lock().unwrap_or_else(|p| p.into_inner()).push_str(s)
        }));
        // Standard input is the protocol. A program that reads the console
        // under the debugger sees the end of input rather than stealing it.
        vm.io.input = Some(Box::new(|| None));
        vm.start(entry);

        Ok(Session {
            image,
            debug,
            vm,
            files,
            vars,
            boundary,
            breakpoint: vec![false; n],
            output,
            mode: Mode::Continue,
            step_from: None,
            resumed: false,
            has_stopped: false,
            finished: None,
            failed_at: None,
            refs: Vec::new(),
            entry,
        })
    }

    /// What the program has written since the last call.
    pub fn take_output(&mut self) -> String {
        std::mem::take(&mut *self.output.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn is_finished(&self) -> bool {
        self.finished.is_some()
    }

    // --- breakpoints ------------------------------------------------------

    /// Replace the breakpoints in the file at `path` with ones on `lines`.
    ///
    /// Answers, per requested line, the line the breakpoint really landed on --
    /// the first line at or just after it with code -- or `None` when there is
    /// none nearby.
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

        // The first boundary of each line in each block, by line. Only the
        // first: a line holding a `let` and the call inside it has two, and a
        // breakpoint that stopped at both would take two presses to leave.
        let mut by_line: HashMap<u32, Vec<Pc>> = HashMap::new();
        let mut seen: HashSet<(Pc, u32)> = HashSet::new();
        for (pc, &b) in self.boundary.iter().enumerate() {
            if !b {
                continue;
            }
            let Some(loc) = self.debug.loc(pc as Pc) else {
                continue;
            };
            if !ids.contains(&loc.source) {
                continue;
            }
            let line = self.files[&loc.source].line_of(loc.span.start);
            let block = self.debug.region(pc as Pc).map_or(0, |r| r.entry);
            if seen.insert((block, line)) {
                by_line.entry(line).or_default().push(pc as Pc);
            }
        }

        for (pc, slot) in self.breakpoint.iter_mut().enumerate() {
            if let Some(loc) = self.debug.loc(pc as Pc) {
                if ids.contains(&loc.source) {
                    *slot = false;
                }
            }
        }

        lines
            .iter()
            .map(|&line| {
                let (landed, pcs) =
                    (line..line + 3).find_map(|l| by_line.get(&l).map(|p| (l, p)))?;
                for &pc in pcs {
                    self.breakpoint[pc as usize] = true;
                }
                Some(landed)
            })
            .collect()
    }

    // --- running ------------------------------------------------------------

    /// Start running in `mode`; [`Session::run`] does the running.
    pub fn resume(&mut self, mode: Mode) {
        self.refs.clear();
        self.mode = mode;
        self.step_from = self.line_at(self.vm.pc() as Pc);
        self.resumed = self.has_stopped;
        self.vm.pinned.clear();
        if mode != Mode::Continue {
            let here = self.current_return();
            let parent = here.and_then(|k| self.parent_return(k));
            self.vm.pinned.push(here.unwrap_or(Value::Unit));
            self.vm.pinned.push(parent.unwrap_or(Value::Unit));
        }
    }

    /// Get ready to run until the top-level definition `name` in the file
    /// `module` is entered -- where debugging a function wants to begin.
    /// [`Session::run`] does the running. `false` if `module` is not one of the
    /// program's files, in which case nothing changes.
    pub fn resume_until_in(&mut self, name: &str, module: &Path) -> bool {
        let want = std::fs::canonicalize(module).unwrap_or_else(|_| module.to_path_buf());
        let source = self.files.iter().find_map(|(id, f)| {
            let p = f.path.as_ref()?;
            (std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == want).then_some(*id)
        });
        let Some(source) = source else { return false };
        self.resume(Mode::Enter(InternedString::from(name), source));
        true
    }

    /// Stop at the first boundary, as a launch with `stopOnEntry` wants.
    pub fn stop_at_entry(&mut self) -> Option<Stop> {
        self.resume(Mode::StepIn);
        self.resumed = false;
        self.step_from = None;
        match self.run(u64::MAX) {
            Some(Stop::Step) => Some(Stop::Entry),
            other => other,
        }
    }

    /// Run at most `budget` instructions. `None` means it is still going.
    pub fn run(&mut self, budget: u64) -> Option<Stop> {
        if let Some(done) = &self.finished {
            return Some(done.clone());
        }
        for _ in 0..budget {
            let at = self.vm.pc() as Pc;
            if !std::mem::replace(&mut self.resumed, false) {
                if let Some(stop) = self.should_stop(at) {
                    self.refs.clear();
                    self.has_stopped = true;
                    return Some(stop);
                }
            }
            match self.vm.step() {
                Ok(None) => {}
                Ok(Some(v)) => {
                    let done = Stop::Exited(Ok(self.vm.show(v)));
                    self.finished = Some(done.clone());
                    self.refs.clear();
                    return Some(done);
                }
                Err(e) => {
                    self.failed_at = Some(at);
                    self.finished = Some(Stop::Exited(Err(e.msg.clone())));
                    self.refs.clear();
                    return Some(Stop::Exception(e.msg));
                }
            }
        }
        None
    }

    fn should_stop(&self, pc: Pc) -> Option<Stop> {
        let i = pc as usize;
        if self.breakpoint.get(i).copied().unwrap_or(false) {
            return Some(Stop::Breakpoint);
        }
        if !self.boundary.get(i).copied().unwrap_or(false) {
            return None;
        }
        // Steps are by line, as a person reads code -- but a call that begins
        // on the line it was made from is still somewhere new, so a change of
        // frame counts as moving too.
        let moved = self.line_at(pc) != self.step_from;
        match self.mode {
            Mode::Continue => None,
            Mode::Enter(name, source) => (self.debug.region(pc).is_some_and(|r| r.name == name)
                && self.debug.loc(pc).is_some_and(|l| l.source == source))
            .then_some(Stop::Entry),
            Mode::StepIn => {
                let other_frame = self
                    .current_return()
                    .is_some_and(|k| k != self.vm.pinned[0]);
                (moved || other_frame).then_some(Stop::Step)
            }
            Mode::StepOver => {
                let k = self.current_return()?;
                let same = k == self.vm.pinned[0];
                let returned = k == self.vm.pinned[1];
                ((same && moved) || returned).then_some(Stop::Step)
            }
            Mode::StepOut => {
                let k = self.current_return()?;
                (k == self.vm.pinned[1]).then_some(Stop::Step)
            }
        }
    }

    /// The source and line of the instruction at `pc`.
    fn line_at(&self, pc: Pc) -> Option<(u32, u32)> {
        let loc = self.debug.loc(pc)?;
        Some((
            loc.source,
            self.files.get(&loc.source)?.line_of(loc.span.start),
        ))
    }

    /// The continuation the code at the current instruction will answer.
    fn current_return(&self) -> Option<Value> {
        let pc = self.here();
        self.debug
            .env(pc)
            .iter()
            .find(|(name, _)| self.debug.returns.contains(name))
            .and_then(|(_, r)| self.described(pc, *r))
    }

    /// The return continuation of whoever called the function `k` belongs to.
    ///
    /// A continuation that captures a function's return continuation is that
    /// function's; one that captures only another of the function's own
    /// continuations is still inside the same call -- `f (g x)` waiting for `g`
    /// before it waits for `f` -- so the walk goes through it.
    ///
    /// One that captures neither is the body of a `handle`: where its value
    /// goes is in a `Ref` it captures, not in the continuation itself, because
    /// a resumption can move it -- see the evidence passing in `meadow_seq`.
    /// The walk goes on from whatever that `Ref` holds.
    fn parent_return(&self, k: Value) -> Option<Value> {
        let mut k = k;
        // A chain within one call is as long as its nesting, so this is bounded
        // by the program's shape; the cap is for a cycle nothing should build.
        for _ in 0..10_000 {
            let (region, a) = self.continuation(k)?;
            let heap = self.vm.heap();
            let named = |set: &HashSet<u32>| {
                (0..heap.len(a))
                    .find(|&i| region.params.get(i).is_some_and(|p| set.contains(p)))
                    .map(|i| heap.field(a, i))
            };
            if let Some(caller) = named(&self.debug.returns) {
                return Some(caller);
            }
            k = match named(&self.debug.continuations) {
                Some(inner) => inner,
                None => (0..heap.len(a)).map(|i| heap.field(a, i)).find_map(|f| {
                    let r = f
                        .addr()
                        .filter(|r| heap.is_object(*r) && heap.kind(*r) == Kind::Ref)?;
                    let target = heap.field(r, 0);
                    self.continuation(target).map(|_| target)
                })?,
            };
        }
        None
    }

    /// Where invoking the closure or frame at `a` goes: a frame carries its
    /// pc, a closure a method table.
    fn entry_of(&self, heap: &meadow_glade::heap::Heap, a: u32) -> Option<u32> {
        if heap.kind(a) == Kind::Frame {
            Some(heap.meta(a))
        } else {
            self.image
                .methods
                .get(heap.meta(a) as usize)
                .and_then(|t| t.first())
                .copied()
        }
    }

    /// `k`'s method, if it is a continuation this program built.
    fn continuation(&self, k: Value) -> Option<(&'static meadow_bytecode::Region, u32)> {
        let a = k.addr()?;
        let heap = self.vm.heap();
        if !heap.is_object(a) || !matches!(heap.kind(a), Kind::Closure | Kind::Frame) {
            return None;
        }
        // A frame's `meta` is its return pc; a closure's is its method table.
        let entry = if heap.kind(a) == Kind::Frame {
            heap.meta(a)
        } else {
            let table = heap.meta(a) as usize;
            // Table 0 is the machine's own halt continuation: the bottom of the
            // stack.
            if table == 0 {
                return None;
            }
            *self.image.methods.get(table)?.first()?
        };
        Some((self.debug.region_at(entry)?, a))
    }

    /// The instruction a stopped program is at: the next one to run, or the
    /// one that failed.
    fn here(&self) -> Pc {
        self.failed_at.unwrap_or(self.vm.pc() as Pc)
    }

    /// What register `r` holds at `pc`, when the debug information can say: a
    /// register is only a word, and what it is comes from the name there and
    /// that name's descriptor.
    fn described(&self, pc: Pc, r: Reg) -> Option<Value> {
        let env = self.debug.env(pc);
        let (name, _) = env.iter().find(|(_, x)| *x == r)?;
        let d = match *self.debug.descs.get(name)? {
            NameDesc::Known(d) => d,
            NameDesc::Var(held) => {
                let (_, dr) = env.iter().find(|(n, _)| *n == held)?;
                self.vm.register(*dr as usize) as meadow_compiler::core::desc::Desc
            }
        };
        (d != meadow_compiler::core::desc::ANY && (0..16).contains(&d))
            .then(|| self.vm.register_as(r as usize, d))
    }

    // --- looking ------------------------------------------------------------

    /// The call stack, innermost first, at most `max` deep.
    pub fn frames(&self, max: usize) -> Vec<Frame> {
        let pc = self.here();
        let mut out = Vec::new();
        let env = self.debug.env(pc);
        out.push(Frame {
            name: self
                .debug
                .region(pc)
                .map_or("?".to_string(), |r| display_name(&r.name)),
            loc: self.debug.loc(pc).or_else(|| self.nearest_loc(pc)),
            vars: env
                .iter()
                .filter_map(|(n, r)| Some((*n, self.described(pc, *r)?)))
                .collect(),
        });
        let mut k = self.current_return();
        while out.len() < max {
            let Some((region, a)) = k.and_then(|k| self.continuation(k)) else {
                break;
            };
            let heap = self.vm.heap();
            let captures = heap.len(a);
            let vars = (0..captures)
                .filter_map(|i| region.params.get(i).map(|p| (*p, heap.field(a, i))))
                .collect();
            out.push(Frame {
                name: display_name(&region.name),
                // Where it is waiting: the call that made the continuation.
                loc: region.origin.or_else(|| self.nearest_loc(region.entry)),
                vars,
            });
            k = self.parent_return(k.expect("walked above"));
        }
        out
    }

    /// The operations the handlers in scope answer, innermost first: the
    /// evidence the running code was handed, which is a list of `#ev(key,
    /// clause, target, rest)` entries in one of its registers.
    fn handled(&self) -> Vec<(String, Value)> {
        let heap = self.vm.heap();
        let entry = |v: Value| {
            v.addr().filter(|a| {
                heap.is_object(*a)
                    && heap.kind(*a) == Kind::Data
                    && self
                        .image
                        .ctors
                        .get(heap.meta(*a) as usize)
                        .is_some_and(|c| matches!(&**c, "#ev" | "#evt"))
            })
        };
        let pc = self.here();
        let Some(mut at) = (0..self.vm.live()).find_map(|r| entry(self.described(pc, r as Reg)?))
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        loop {
            let key = match heap.field(at, 0) {
                Value::Str(s) => s.to_string(),
                other => other.kind().to_string(),
            };
            out.push((key, heap.field(at, 1)));
            match entry(heap.field(at, 3)) {
                Some(next) => at = next,
                None => return out,
            }
        }
    }

    /// The first position at or after `pc` in its block.
    fn nearest_loc(&self, pc: Pc) -> Option<Loc> {
        let end = self.debug.region(pc).map_or(pc + 1, |r| r.end);
        (pc..end).find_map(|p| self.debug.loc(p))
    }

    pub fn file(&self, loc: Loc) -> Option<&SourceFile> {
        self.files.get(&loc.source)
    }

    /// The scopes of frame `frame`.
    pub fn scopes(&mut self, frame: usize) -> Vec<Scope> {
        let mut out = vec![Scope {
            name: "Locals",
            reference: self.reference(Target::Locals(frame)),
            expensive: false,
        }];
        if frame == 0 {
            out.push(Scope {
                name: "Registers",
                reference: self.reference(Target::Registers),
                expensive: true,
            });
            out.push(Scope {
                name: "Handlers",
                reference: self.reference(Target::Handlers),
                expensive: true,
            });
            out.push(Scope {
                name: "Heap",
                reference: self.reference(Target::Heap),
                expensive: true,
            });
        }
        out
    }

    fn reference(&mut self, t: Target) -> u32 {
        self.refs.push(t);
        self.refs.len() as u32
    }

    /// The rows behind a reference from [`Session::scopes`] or a [`Variable`].
    pub fn variables(&mut self, reference: u32) -> Vec<Variable> {
        let Some(&target) = self.refs.get(reference.wrapping_sub(1) as usize) else {
            return Vec::new();
        };
        match target {
            Target::Locals(frame) => {
                let Some(f) = self.frames(frame + 1).into_iter().nth(frame) else {
                    return Vec::new();
                };
                let mut seen = HashSet::new();
                let mut rows = Vec::new();
                // Newest first, so a shadowed name shows its current binding.
                for (name, value) in f.vars {
                    let Some(info) = self.vars.get(&name).cloned() else {
                        continue;
                    };
                    if !seen.insert(info.name.clone()) {
                        continue;
                    }
                    rows.push(self.row(info.name, value, Some(info.ty)));
                }
                rows
            }
            Target::Registers => {
                let pc = self.here();
                let names: HashMap<u8, u32> =
                    self.debug.env(pc).iter().map(|(n, r)| (*r, *n)).collect();
                (0..self.vm.live())
                    .map(|r| {
                        let label = match names.get(&(r as u8)).and_then(|n| self.vars.get(n)) {
                            Some(info) => format!("r{r} ({})", info.name),
                            None => format!("r{r}"),
                        };
                        match self.described(pc, r as Reg) {
                            Some(v) => self.row(label, v, None),
                            None => Variable {
                                name: label,
                                value: format!("{:#x}", self.vm.register(r)),
                                ty: None,
                                children: 0,
                            },
                        }
                    })
                    .collect()
            }
            Target::Handlers => self
                .handled()
                .into_iter()
                .enumerate()
                .map(|(i, (key, clause))| {
                    let mut row = self.row(format!("#{i}"), clause, None);
                    row.value = format!("handles {key}");
                    row
                })
                .collect(),
            Target::Heap => {
                let heap = self.vm.heap();
                let (collections, allocated) = (heap.collections, heap.allocated);
                let (used, capacity) = (heap.used(), heap.capacity());
                let plain = |name: &str, value: String| Variable {
                    name: name.to_string(),
                    value,
                    ty: None,
                    children: 0,
                };
                vec![
                    plain("used", format!("{used} slots")),
                    plain("capacity", format!("{capacity} slots")),
                    plain("collections", collections.to_string()),
                    plain("allocated", format!("{allocated} slots in total")),
                    plain("instructions", self.vm.steps.to_string()),
                    plain("pc", self.here().to_string()),
                ]
            }
            Target::Value(v) => self.children(v),
        }
    }

    /// Look a name up in frame `frame`, the way a hover or a watch asks.
    pub fn evaluate(&mut self, frame: usize, expr: &str) -> Option<Variable> {
        let f = self.frames(frame + 1).into_iter().nth(frame)?;
        let name = expr.trim();
        let (_, value) = f
            .vars
            .iter()
            .find(|(n, _)| self.vars.get(n).is_some_and(|i| i.name == name))?;
        let ty = self
            .vars
            .values()
            .find(|i| i.name == name)
            .map(|i| i.ty.clone());
        Some(self.row(name.to_string(), *value, ty))
    }

    fn row(&mut self, name: String, v: Value, ty: Option<String>) -> Variable {
        // A generic function's variable has a type variable for its static
        // type. The value knows what it is, for the kinds that carry it -- the
        // numbers, which a function generic over its number type is full of.
        let ty = match ty {
            Some(t) if is_type_variable(&t) => match v {
                Value::Obj(a)
                    if self.vm.heap().is_object(a) && self.vm.heap().kind(a) == Kind::BigInt =>
                {
                    Some("BigInt".to_string())
                }
                Value::Obj(_) => Some(t),
                other => Some(other.kind().to_string()),
            },
            other => other,
        };
        let children = if self.has_children(v) {
            self.reference(Target::Value(v))
        } else {
            0
        };
        Variable {
            name,
            value: self.preview(v, 5),
            ty,
            children,
        }
    }

    fn has_children(&self, v: Value) -> bool {
        let Some(a) = v.addr() else { return false };
        let heap = self.vm.heap();
        heap.is_object(a)
            && heap.len(a) > 0
            && matches!(
                heap.kind(a),
                Kind::Data
                    | Kind::Array
                    | Kind::Bytes
                    | Kind::MutArray
                    | Kind::Record
                    | Kind::Closure
                    | Kind::Frame
                    | Kind::Ref
                    | Kind::Compact
            )
    }

    fn children(&mut self, v: Value) -> Vec<Variable> {
        let Some(a) = v.addr() else { return Vec::new() };
        let heap = self.vm.heap();
        if !heap.is_object(a) {
            return Vec::new();
        }
        let n = heap.len(a);
        let fields: Vec<Value> = match heap.kind(a) {
            Kind::Array | Kind::Bytes => (0..heap.array_len(a).min(1000))
                .map(|i| heap.array_value(a, i))
                .collect(),
            _ => (0..n.min(1000)).map(|i| heap.field(a, i)).collect(),
        };
        match heap.kind(a) {
            Kind::Record => fields
                .chunks(2)
                .map(|pair| {
                    let label = match pair[0] {
                        Value::Str(s) => s.to_string(),
                        _ => "?".to_string(),
                    };
                    self.row(label, pair.get(1).copied().unwrap_or(Value::Unit), None)
                })
                .collect(),
            Kind::Array | Kind::Bytes | Kind::MutArray => fields
                .into_iter()
                .enumerate()
                .map(|(i, f)| self.row(format!("[{i}]"), f, None))
                .collect(),
            Kind::Closure | Kind::Frame => {
                let entry = self.entry_of(heap, a);
                let params = entry
                    .and_then(|pc| self.debug.region_at(pc))
                    .map(|r| r.params.clone())
                    .unwrap_or_default();
                fields
                    .into_iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let name = params
                            .get(i)
                            .and_then(|p| self.vars.get(p))
                            .map_or_else(|| format!("capture {i}"), |v| v.name.clone());
                        self.row(name, f, None)
                    })
                    .collect()
            }
            Kind::Ref | Kind::Compact => fields
                .into_iter()
                .map(|f| self.row("contents".to_string(), f, None))
                .collect(),
            _ => fields
                .into_iter()
                .enumerate()
                .map(|(i, f)| self.row(i.to_string(), f, None))
                .collect(),
        }
    }

    /// A short rendering of `v`, `depth` levels deep at most.
    fn preview(&self, v: Value, depth: usize) -> String {
        let mut out = String::new();
        self.preview_into(&mut out, v, depth, &mut 60);
        out
    }

    fn preview_into(&self, out: &mut String, v: Value, depth: usize, budget: &mut usize) {
        if *budget == 0 {
            out.push('…');
            return;
        }
        *budget -= 1;
        let a = match v {
            Value::Obj(a) => a,
            Value::Str(s) => {
                let text: String = s.chars().take(80).collect();
                out.push_str(&format!("{text:?}"));
                return;
            }
            other => {
                out.push_str(&self.vm.show(other));
                return;
            }
        };
        let heap = self.vm.heap();
        if !heap.is_object(a) {
            out.push_str("<dangling>");
            return;
        }
        let n = heap.len(a);
        match heap.kind(a) {
            Kind::Str => {
                let bytes = heap.packed_bytes_in(a, 0, 4 * 80);
                let text: String = String::from_utf8_lossy(&bytes).chars().take(80).collect();
                out.push_str(&format!("{text:?}"));
            }
            Kind::BigInt => out.push_str(&self.vm.show(v)),
            Kind::Resume | Kind::Stack => out.push_str("<resumption>"),
            Kind::Channel => out.push_str("<channel>"),
            Kind::Task => out.push_str("<thread>"),
            Kind::TVar => out.push_str("<tvar>"),
            Kind::Ref => {
                out.push_str("ref ");
                self.nested(out, heap.field(a, 0), depth, budget);
            }
            Kind::Compact => {
                out.push_str("compact ");
                self.nested(out, heap.field(a, 0), depth, budget);
            }
            Kind::Closure | Kind::Frame => {
                let entry = self.entry_of(heap, a);
                let name = entry
                    .and_then(|pc| self.debug.region_at(pc))
                    .map(|r| r.name.to_string());
                match name {
                    Some(name) => out.push_str(&format!("<function in {name}>")),
                    None => out.push_str("<function>"),
                }
            }
            Kind::Array | Kind::Bytes | Kind::MutArray => {
                if heap.kind(a) == Kind::MutArray {
                    out.push_str("mut ");
                }
                out.push_str("#[");
                let n = if heap.kind(a) == Kind::MutArray {
                    n
                } else {
                    heap.array_len(a)
                };
                self.list(
                    out,
                    (0..n).map(|i| {
                        if heap.kind(a) == Kind::MutArray {
                            heap.field(a, i)
                        } else {
                            heap.array_value(a, i)
                        }
                    }),
                    depth,
                    budget,
                );
                out.push(']');
            }
            Kind::Record => {
                out.push_str("{ ");
                for j in 0..n / 2 {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    if *budget == 0 || depth == 0 {
                        out.push('…');
                        break;
                    }
                    if let Value::Str(l) = heap.field(a, 2 * j) {
                        out.push_str(&format!("{l} = "));
                    }
                    self.preview_into(out, heap.field(a, 2 * j + 1), depth - 1, budget);
                }
                out.push_str(" }");
            }
            Kind::Data => {
                let ctor = self
                    .image
                    .ctor(heap.meta(a))
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                let bare = ctor.rsplit('.').next().unwrap_or(&ctor).to_string();
                if ctor == "#tuple" {
                    out.push('(');
                    self.list(out, (0..n).map(|i| heap.field(a, i)), depth, budget);
                    out.push(')');
                } else if ctor == "List.Cons" || ctor == "List.Nil" {
                    out.push('[');
                    let mut cur = a;
                    let mut first = true;
                    while heap.is_object(cur)
                        && self
                            .image
                            .ctor(heap.meta(cur))
                            .is_some_and(|c| &*c == "List.Cons")
                    {
                        if !first {
                            out.push_str("; ");
                        }
                        first = false;
                        if *budget == 0 || depth == 0 {
                            out.push('…');
                            break;
                        }
                        self.preview_into(out, heap.field(cur, 0), depth - 1, budget);
                        match heap.field(cur, 1) {
                            Value::Obj(next) => cur = next,
                            _ => break,
                        }
                    }
                    if first {
                        out.push(';');
                    }
                    out.push(']');
                } else if n == 0 {
                    out.push_str(&bare);
                } else {
                    out.push_str(&bare);
                    for i in 0..n {
                        out.push(' ');
                        self.nested(out, heap.field(a, i), depth, budget);
                    }
                }
            }
        }
    }

    /// A value inside another, parenthesised when it has parts.
    fn nested(&self, out: &mut String, v: Value, depth: usize, budget: &mut usize) {
        if depth == 0 {
            out.push('…');
            return;
        }
        let wrap = v.addr().is_some_and(|a| {
            let heap = self.vm.heap();
            heap.is_object(a)
                && heap.kind(a) == Kind::Data
                && heap.len(a) > 0
                && !self
                    .image
                    .ctor(heap.meta(a))
                    .is_some_and(|c| &*c == "#tuple" || &*c == "List.Cons")
        });
        if wrap {
            out.push('(');
        }
        self.preview_into(out, v, depth - 1, budget);
        if wrap {
            out.push(')');
        }
    }

    fn list(
        &self,
        out: &mut String,
        items: impl Iterator<Item = Value>,
        depth: usize,
        budget: &mut usize,
    ) {
        for (i, v) in items.enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            if *budget == 0 || depth == 0 {
                out.push('…');
                return;
            }
            self.preview_into(out, v, depth - 1, budget);
        }
    }
}

/// `path` built for debugging, starting at `entry` if there is one: the
/// program as AxCut, with where everything was written kept, and the packages
/// it was built from -- what either engine's session starts from.
pub(super) fn build(
    path: &Path,
    entry: Option<&Entry>,
) -> Result<(meadow_seq::Program, Vec<meadow_compiler::CompiledPackage>), LaunchError> {
    build_reloadable(path, entry, false)
}

/// [`build`], lowered -- if `reloadable` -- so that a new build may be loaded
/// into it as it runs, as an interpreted build is (`aot::lowered_for`): what
/// Silo's interpreter runs under a debugger.
pub(super) fn build_reloadable(
    path: &Path,
    entry: Option<&Entry>,
    reloadable: bool,
) -> Result<(meadow_seq::Program, Vec<meadow_compiler::CompiledPackage>), LaunchError> {
    let path =
        std::fs::canonicalize(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut options = Resolved::new(Profile::Debug).options;
    options.debug_info = true;
    // The entry below is run, like `main`, so it may perform effects.
    options.entry_name = Some(ENTRY);

    // The entry is a definition added to the end of the function's own
    // module, so the expression sees exactly what that module sees --
    // private functions, and whatever it `use`s.
    let added = entry.map(|e| format!("def {ENTRY} =\n  {}", e.expression));
    let original_len = entry
        .and_then(|e| std::fs::read_to_string(&e.module).ok())
        .map_or(usize::MAX, |t| t.len());
    let addition = entry
        .zip(added.as_deref())
        .map(|(e, text)| pipeline::Addition {
            file: &e.module,
            text,
        });
    let out = pipeline::build_with(&path, options, addition);
    if !out.diagnostics.is_empty() {
        let shown: Vec<String> = out
            .diagnostics
            .iter()
            .take(10)
            .map(|d| {
                // Past the end of the file is the text we added.
                if d.label.1.start as usize >= original_len {
                    format!(
                        "in `{}`: {}",
                        entry.map_or("", |e| e.expression.as_str()),
                        d.msg
                    )
                } else {
                    format!("{}: {}", plain_path(&d.filename), d.msg)
                }
            })
            .collect();
        return Err(format!(
            "{} did not compile:\n{}",
            path_display(&path),
            shown.join("\n")
        ));
    }
    let mut linked = out.linked.ok_or("the build produced no program")?;
    if entry.is_some() {
        let var = linked
            .program
            .defs
            .iter()
            .find(|d| &*d.name == ENTRY)
            .map(|d| d.var)
            .ok_or("the entry definition went missing in the build")?;
        linked.program.entry = Some(var);
    }
    if linked.program.entry.is_none() {
        return Err(format!("{} has no `main` to run", path_display(&path)));
    }
    let lowered = meadow_seq::lower_program_with(&linked.program, options.opt, reloadable);
    if !lowered.unsupported.is_empty() {
        return Err(format!(
            "the back end cannot compile this program: {:?}",
            lowered.unsupported
        ));
    }
    Ok((lowered.program, linked.packages))
}

/// Where each of `packages`' sources is, and what each name it binds is
/// called and typed: the debug information a program's own does not hold.
/// `origins` maps a specialized copy's variables to the originals'.
pub(super) fn sources(
    packages: &[meadow_compiler::CompiledPackage],
    origins: impl IntoIterator<Item = (u32, u32)>,
) -> (HashMap<u32, SourceFile>, HashMap<u32, VarInfo>) {
    let std_root = crate::stdlib::extract_sources();
    let mut files = HashMap::new();
    let mut vars = HashMap::new();
    for pkg in packages {
        for m in &pkg.modules {
            let name = match m.source.kind {
                SourceKind::File(n) => n.to_string(),
                SourceKind::Interactive => "<interactive>".to_string(),
            };
            let path = {
                let p = Path::new(&name);
                if p.is_absolute() {
                    Some(PathBuf::from(plain_path(&name)))
                } else {
                    std_root.as_ref().map(|r| r.join(p)).filter(|p| p.is_file())
                }
            };
            files.insert(m.source.id, SourceFile::new(path, name, m.source.content));
            collect_names(&m.hir, &m.source.content, &pkg.types, &mut vars);
        }
    }
    // A specialized copy's variables go by the names of the originals.
    for (copy, original) in origins {
        if let Some(info) = vars.get(&original).cloned() {
            vars.insert(copy, info);
        }
    }
    (files, vars)
}

/// Every name a module binds: its spelling, from the source, and its type.
fn collect_names(
    module: &hir::LModule,
    text: &str,
    types: &meadow_compiler::infer::TypeTable,
    out: &mut HashMap<u32, VarInfo>,
) {
    let mut names = Names { text, types, out };
    for d in &module.value().decls {
        if let hir::Decl::Bind(b) = d.value() {
            names.bind(b);
        }
    }
}

struct Names<'a> {
    text: &'a str,
    types: &'a meadow_compiler::infer::TypeTable,
    out: &'a mut HashMap<u32, VarInfo>,
}

impl Names<'_> {
    fn ident(&mut self, id: &hir::Ident) {
        let span = id.span;
        let Some(name) = self.text.get(span.start as usize..span.end as usize) else {
            return;
        };
        let ty = self
            .types
            .get(id.id)
            .map(|t| Renderer::for_table(self.types).render(t))
            .unwrap_or_default();
        self.out.insert(
            id.value().0,
            VarInfo {
                name: name.to_string(),
                ty,
            },
        );
    }

    fn bind(&mut self, b: &hir::Bind) {
        match b {
            hir::Bind::Fun(name, params, _, body) => {
                self.ident(name);
                params.iter().for_each(|p| self.pat(p));
                self.expr(body);
            }
            hir::Bind::Pat(p, e) => {
                self.pat(p);
                self.expr(e);
            }
            hir::Bind::Error => {}
        }
    }

    fn pat(&mut self, p: &hir::LPat) {
        match p.value() {
            hir::Pat::Var(id) => {
                // A pattern variable's type is recorded on the pattern.
                let span = id.span;
                if let Some(name) = self.text.get(span.start as usize..span.end as usize) {
                    let ty = self
                        .types
                        .get(p.id)
                        .or_else(|| self.types.get(id.id))
                        .map(|t| Renderer::for_table(self.types).render(t))
                        .unwrap_or_default();
                    self.out.insert(
                        id.value().0,
                        VarInfo {
                            name: name.to_string(),
                            ty,
                        },
                    );
                }
            }
            hir::Pat::As(id, sub) => {
                self.ident(id);
                self.pat(sub);
            }
            hir::Pat::Ann(inner, _) => self.pat(inner),
            hir::Pat::Cons(_, ps)
            | hir::Pat::Tuple(ps)
            | hir::Pat::Array(ps)
            | hir::Pat::List(ps) => ps.iter().for_each(|x| self.pat(x)),
            hir::Pat::Record(fs, _) => fs.iter().for_each(|(_, x)| self.pat(x)),
            hir::Pat::View(f, x) => {
                self.expr(f);
                self.pat(x);
            }
            hir::Pat::Wildcard | hir::Pat::Unit | hir::Pat::Lit(_) | hir::Pat::Error => {}
        }
    }

    fn expr(&mut self, e: &hir::LExpr) {
        match e.value() {
            hir::Expr::Lam(ps, body) => {
                ps.iter().for_each(|p| self.pat(p));
                self.expr(body);
            }
            hir::Expr::Let(binds, body) => {
                binds.iter().for_each(|b| self.bind(b));
                self.expr(body);
            }
            hir::Expr::Match(s, arms) => {
                self.expr(s);
                for (p, g, b) in arms {
                    self.pat(p);
                    if let Some(g) = g {
                        self.expr(g);
                    }
                    self.expr(b);
                }
            }
            hir::Expr::Handle(body, arms, ret) => {
                self.expr(body);
                for arm in arms {
                    self.pat(&arm.param);
                    self.ident(&arm.resume);
                    self.expr(&arm.body);
                }
                if let Some((p, b)) = ret {
                    self.pat(p);
                    self.expr(b);
                }
            }
            hir::Expr::App(f, args) => {
                self.expr(f);
                args.iter().for_each(|a| self.expr(a));
            }
            hir::Expr::If(c, t, f) => {
                self.expr(c);
                self.expr(t);
                self.expr(f);
            }
            hir::Expr::Tuple(xs)
            | hir::Expr::Array(xs)
            | hir::Expr::List(xs)
            | hir::Expr::Cons(_, xs) => xs.iter().for_each(|x| self.expr(x)),
            hir::Expr::Record(fs, base) => {
                fs.iter().for_each(|(_, x)| self.expr(x));
                if let Some(b) = base {
                    self.expr(b);
                }
            }
            hir::Expr::Field(o, _) => self.expr(o),
            hir::Expr::Update(base, fs) => {
                self.expr(base);
                fs.iter().for_each(|(_, e)| self.expr(e));
            }
            hir::Expr::Var(_) | hir::Expr::Lit(_) | hir::Expr::Unit | hir::Expr::Error => {}
        }
    }
}

/// A definition's name as a stack frame shows it.
pub(super) fn display_name(name: &str) -> String {
    if name == ENTRY {
        "(debug entry)".to_string()
    } else {
        name.to_string()
    }
}

/// A path as a person writes it: without the `\\?\` prefix `canonicalize`
/// gives on Windows, which an editor would not recognise as the same file.
pub fn plain_path(p: &str) -> String {
    match p.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_string(),
        },
        None => p.to_string(),
    }
}

pub(super) fn path_display(p: &Path) -> String {
    plain_path(&p.display().to_string())
}

/// `a`, `n1`: a type that is only a variable.
pub(super) fn is_type_variable(ty: &str) -> bool {
    let mut chars = ty.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase()) && chars.all(|c| c.is_ascii_alphanumeric())
}
