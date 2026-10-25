//! **Linear AxCut to LLVM IR**, as text.
//!
//! One LLVM function per block entered from elsewhere -- a definition, a
//! method -- in LLVM's GHC convention, taking the environment as `i64`s and
//! answering an `i64`. Everything else is basic blocks: a `switch`'s arms and a
//! primitive's continuations branch, and a renaming emits nothing. `jump` and
//! `invoke` are tail calls with every argument in a register: see
//! [`CallConv`] for why that convention, and [`CallConv::regs`] for what
//! becomes of an argument past the last register.
//!
//! A **frame** (`docs/SILO.md`, "Frames are the native stack") is not built
//! while it is only waiting to be the continuation of a call: the call is an
//! LLVM `call` with a null continuation, and the frame's method is emitted
//! after it. Anything else done with a frame -- sharing it, storing it --
//! builds it then, as the object it would have been.
//!
//! Memory is the paper's (see `docs/SILO.md`): a block's first word counts the
//! references besides one; sharing adds to it, erasing takes from it or, at
//! zero, hands the block to the runtime's free list; loading fields out of a
//! block whose count is zero takes them without touching a count.

use crate::linear::{L, LBlock, SwitchArm};
use meadow_axcut::config::{Config, Memory, Stack};
use meadow_axcut::{Extern, Name, Program, Rep, VarId};
use meadow_rt::desc;
use meadow_rt::layout::{Field, Layout};
use meadow_rt::{Lit, Prim};
use std::collections::HashMap;
use std::fmt::Write;
use std::rc::Rc;

/// What the shape of the emitted code takes for granted of a layout, beyond
/// the numbers it reads from it: that a small value is one whose single tag
/// bit is set; that a uniform block's bit and its one descriptor are in one
/// word; and that descriptors are packed so that a shift finds one. And of a
/// layout that counts, what counting's code takes for granted: that a block's
/// count is the integer its address points at; that one store writes a new
/// block's count and length, and one its kind and `meta`, as the cycle
/// collector's bits are; that a reused block's descriptors are words of their
/// own; and that a shift alone reads the length and the `meta`.
fn suits(l: &Layout) {
    assert!(
        l.imm_mask.is_power_of_two() && l.imm_tag == l.imm_mask,
        "a small value is told by one bit"
    );
    assert_eq!(
        l.uniform.word, l.elem_desc.word,
        "a uniform block's descriptor is beside the bit"
    );
    assert!(
        l.descs_per_word.is_power_of_two() && l.desc_bits.is_power_of_two(),
        "descriptors are found by shifting"
    );
    if !l.counted {
        return;
    }
    assert!(
        l.count.word == 0 && l.count.shift == 0 && [8, 16, 32, 64].contains(&l.count.bits),
        "the count is the integer at a block's address"
    );
    assert_eq!(l.len.word, l.count.word, "the length is beside the count");
    for f in [l.meta, l.elem_desc, l.uniform, l.colour, l.mutable] {
        assert_eq!(f.word, l.kind.word, "the kind's word holds the rest");
    }
    assert_eq!(l.inline_descs, 0, "descriptors are words of their own");
    for f in [l.len, l.meta] {
        assert!(f.tops(), "the length and `meta` top their words");
    }
}

/// The instructions that read `field` out of its word `w`, in `f`: a shift
/// unless it is at the bottom, and a mask unless it is at the top. Its value.
fn read_field(f: &mut Fun, w: &str, field: Field) -> String {
    let mut x = w.to_string();
    if field.shift > 0 {
        let s = f.t();
        f.i(format!("{s} = lshr i64 {x}, {}", field.shift));
        x = s;
    }
    if !field.tops() {
        let m = f.t();
        f.i(format!("{m} = and i64 {x}, {}", field.mask()));
        x = m;
    }
    x
}

/// The LLVM type of a block's count, as it is loaded and stored: `i32` for
/// Silo's.
fn count_ty(l: &Layout) -> String {
    format!("i{}", l.count.bits)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub msg: String,
}

fn err<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error { msg: msg.into() })
}

/// A name's value, where the emitted code has it.
#[derive(Debug, Clone)]
enum V {
    /// An `i64` operand: a register (`%t3`, `%a0`) or a constant.
    Val(String),
    /// A frame not built (see the module docs).
    Frame(Rc<Frame>),
}

#[derive(Debug)]
struct Frame {
    /// Which, so that its code is written once however it is entered.
    id: usize,
    captured: Vec<V>,
    method: LBlock,
}

/// A descriptor, as the emitted code has it.
#[derive(Debug, Clone, PartialEq)]
enum D {
    Known(i64),
    /// In this register, at run time: a value of a type variable's type.
    Dyn(String),
}

/// A shadow record's descriptor for a reuse token: `silo::shadow::TOKEN`.
const SHADOW_TOKEN: i64 = -1;

/// Whether the program spawns a thread anywhere. Known before anything is
/// emitted, because a program that cannot spawn is emitted differently: no
/// safe points, and a spill area of its own. See [`Module::units`].
fn spawns(program: &Program) -> bool {
    uses(program, &[Prim::ThreadSpawn])
}

/// Whether the program can tie a knot: whether it stores into a block that
/// already exists. Everything else Meadow builds is built bottom-up and
/// points only at what was there before it, so no cycle can come of it --
/// `setRef` and a write into a mutable array are the two ways to make one,
/// and a program with neither needs no cycle collector at all (see
/// `silo/src/cycles.rs`).
///
/// `setField`, which destination-passing uses to fill a hole in a structure
/// being built, is not one of them: what it writes is always newer than what
/// it writes into, so it cannot point backwards.
///
/// What is stored matters as much as where. A store of an `Int`, a `Float` or
/// a `Word` cannot make a cycle whatever it is stored into, and that is not a
/// detail: `Std` builds every string it prints in a mutable array of bytes,
/// so taking the representation into account is what lets an ordinary program
/// -- one that prints, and ties no knots -- have no collector in it at all.
fn ties_knots(program: &Program) -> bool {
    knots(program, &|p, args| {
        let value = match p {
            // `setRef r v`, `stSetArray a i v`.
            Prim::SetRef => args.get(1),
            Prim::StSetArray => args.get(2),
            _ => return false,
        };
        // No representation known is a reference as far as this is concerned:
        // the question is only ever asked to leave the collector out, so what
        // is not known says nothing.
        value.is_none_or(|n| {
            !matches!(
                program.reps.get(n),
                Some(Rep::Int | Rep::Float | Rep::Bits(_))
            )
        })
    })
}

/// Whether the program can ever hold a value inside a compact region, which
/// is whether it makes one: a region comes from `compact`, or holds what a
/// `TVar` holds -- `silo/src/sched.rs` keeps a `TVar`'s value in one, where
/// every thread reads it in place -- and nothing can be read out of one, sent
/// between threads or added to that did not start there. A program with
/// neither in it therefore never meets a block whose count says it is in a
/// region, and its counting helpers do not ask -- see `silo/src/region.rs`,
/// which is where the asking is paid for.
///
/// Leaving out the `TVar`s let a program share a value read from one without
/// telling its region, while the runtime's own erase of the same value told
/// it: the region went while the program still read it.
fn makes_regions(program: &Program) -> bool {
    uses(
        program,
        &[
            Prim::Compact,
            Prim::CompactAdd,
            Prim::StmNew,
            Prim::StmRead,
            Prim::StmWrite,
        ],
    )
}

/// Does the program use any of these primitives anywhere?
fn uses(program: &Program, want: &[Prim]) -> bool {
    knots(program, &|p, _| want.contains(p))
}

/// Is there anywhere in the program a primitive `want` says yes to, given
/// what it is applied to?
fn knots(program: &Program, want: &dyn Fn(&Prim, &[Name]) -> bool) -> bool {
    fn in_statement(s: &meadow_axcut::Statement, want: &dyn Fn(&Prim, &[Name]) -> bool) -> bool {
        use meadow_axcut::Statement::*;
        match s {
            Substitute(_, b) => in_statement(&b.body, want),
            Jump(_) | Invoke(..) | Error(_) => false,
            Let { rest, .. } => in_statement(rest, want),
            Switch { arms, default, .. } => {
                arms.iter().any(|(_, b)| in_statement(&b.body, want))
                    || in_statement(&default.body, want)
            }
            New { methods, rest, .. } => {
                methods.iter().any(|b| in_statement(&b.body, want)) || in_statement(rest, want)
            }
            Extern { op, args, blocks } => {
                let here = match op {
                    meadow_axcut::Extern::Prim(p) => want(p, args),
                    meadow_axcut::Extern::PrimK(p, _) => want(p, args),
                    meadow_axcut::Extern::BranchPrim(p) => want(p, args),
                    _ => false,
                };
                here || blocks.iter().any(|b| in_statement(&b.body, want))
            }
            Mark(_, s) => in_statement(s, want),
        }
    }
    program
        .defs
        .iter()
        .any(|d| in_statement(&d.block.body, want))
}

/// The module being written.
pub struct Module<'p> {
    program: &'p Program,
    /// The calling convention of every function the module defines: see
    /// [`CallConv`].
    cc: &'static str,
    /// How many words that convention passes in registers: see
    /// [`CallConv::regs`].
    reg_words: usize,
    /// Every function written, as text, with how many parameters it takes
    /// in registers -- for declaring it in the other units: see
    /// [`Module::units`].
    funs: Vec<(String, String, usize)>,
    /// The module each of `funs` is of, beside it: see [`Module::in_module`].
    fun_modules: Vec<String>,
    /// The module of the definition being written.
    module: String,
    /// What each label's function is called: see [`Module::name_labels`].
    label_names: HashMap<u32, String>,
    /// The function of the definition being written, less its `@mw.L` --
    /// what its methods and frames are named after -- and how many of each
    /// it has had so far.
    stem: String,
    stem_methods: usize,
    stem_frames: usize,
    /// The definition being written, and how many bytes of functions and how
    /// many functions each definition came to, by module and name: what
    /// [`Module::sizes`] reports.
    def_name: String,
    sizes: HashMap<(String, String), (usize, usize)>,
    /// Every method's function, in method-table order: a closure's `meta` is
    /// the index of its first method here.
    methods: Vec<String>,
    /// Methods still to be written: their function's name, and the block.
    pending: Vec<(String, LBlock, usize)>,
    next_method: usize,
    /// Interned names, as the small integers a `Sym` literal loads.
    syms: Vec<meadow_intern::InternedString>,
    /// Constant strings: their global's name.
    strings: HashMap<String, String>,
    string_defs: String,
    /// Each string's global and its size, for declaring it elsewhere.
    string_sizes: Vec<(String, usize)>,
    /// Frames: how many made, and each one's function and method table, once
    /// written -- see [`Module::frame_fn`].
    next_frame: usize,
    frame_fns: HashMap<usize, String>,
    frame_tables: HashMap<usize, usize>,
    pending_frames: Vec<(String, Rc<Frame>)>,
    /// Whether the program can ever have a second thread: whether it
    /// spawns one anywhere. If it cannot, it pays for none of what threads
    /// need -- see [`Module::units`].
    threaded: bool,
    /// Whether it can make a cycle: see [`ties_knots`]. If it cannot, its
    /// counting helpers keep no candidates and its runtime collects none.
    cycles: bool,
    /// Whether it can make a compact region: see [`makes_regions`]. If it
    /// cannot, no value it ever holds is inside one, so its counting helpers
    /// do not ask.
    regions: bool,
    /// Whether it can capture a continuation, and so abandon one: whether it
    /// performs an operation of a general handler anywhere. If it can, every
    /// frame not built that waits on a call is recorded where a discarded
    /// continuation's segments can find what it holds -- see
    /// `silo/src/shadow.rs`. If it cannot, no record is ever made.
    shadow: bool,
    /// The most words any call passes through the spill area, or any function
    /// reads from it: how big the area has to be. See [`spill`].
    spill_words: usize,
    /// Whether memory is traced rather than counted: see [`Memory`]. Then
    /// nothing is shared or erased, no block is reused, and the program is
    /// collected at safe points, where everything it holds is in a record.
    tracing: bool,
    /// Whether blocks move (`Memory::Generational`): then what a record holds
    /// is read back after the poll or the call it was made for, and a store
    /// into a block that may be older than what it stores tells the runtime
    /// (`silo/src/trace.rs`, "The young generation"). Nothing it does is
    /// written otherwise, so a counted or a traced program's IR is as it was.
    moving: bool,
    /// Whether a function that begins with too little stack left runs on a
    /// new piece of it: see [`Stack`] and [`Module::stack_point`].
    growable: bool,
    /// The trampoline of the function being written, which goes with it:
    /// see [`Module::stack_point`].
    trampoline: Option<String>,
    /// How a block is laid out: the configuration's layout, which the
    /// runtime it is linked with has too (`meadow_rt::layout`). Every word,
    /// shift, mask and kind of a block the code names is taken from it.
    layout: Layout,
}

impl<'p> Module<'p> {
    pub fn new(program: &'p Program, conv: CallConv) -> Module<'p> {
        Module::with(program, conv, Config::default())
    }

    /// [`Module::new`], run as `config` says.
    pub fn with(program: &'p Program, conv: CallConv, config: Config) -> Module<'p> {
        let tracing = config.memory.traced();
        let moving = config.memory == Memory::Generational;
        let layout = config.layout.description();
        suits(&layout);
        Module {
            program,
            cc: conv.keyword(),
            reg_words: conv.regs(),
            funs: Vec::new(),
            fun_modules: Vec::new(),
            module: String::new(),
            label_names: HashMap::new(),
            stem: String::new(),
            stem_methods: 0,
            stem_frames: 0,
            def_name: String::new(),
            sizes: HashMap::new(),
            methods: Vec::new(),
            pending: Vec::new(),
            next_method: 0,
            syms: Vec::new(),
            strings: HashMap::new(),
            string_defs: String::new(),
            string_sizes: Vec::new(),
            next_frame: 0,
            frame_fns: HashMap::new(),
            frame_tables: HashMap::new(),
            pending_frames: Vec::new(),
            threaded: spawns(program),
            // Tracing finds cycles as it finds everything else, and keeps a
            // region alive by finding a pointer into it: neither is counted.
            cycles: !tracing && ties_knots(program),
            regions: !tracing && makes_regions(program),
            // A frame's record is where the collector finds what the frame
            // holds while it waits on a call.
            shadow: tracing || uses(program, &[Prim::Detach]),
            spill_words: 0,
            tracing,
            moving,
            growable: config.stack == Stack::Growable,
            trampoline: None,
            layout,
        }
    }

    /// A global holding `text`'s bytes, and its length.
    fn cstr(&mut self, text: &str) -> (String, usize) {
        if let Some(g) = self.strings.get(text) {
            return (g.clone(), text.len());
        }
        // Named for what it says, not for when it was first said: a number
        // would move with every string before it in the program, and the
        // text of every unit that names this one with it.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        let mut g = format!("@mw.s.{h:016x}");
        // Two texts of one hash: the later is told apart by how many there
        // were before it, which is the same from build to build unless one
        // of the two is new.
        let mut again = 1;
        while self.string_sizes.iter().any(|(had, _)| *had == g) {
            again += 1;
            g = format!("@mw.s.{h:016x}_{again}");
        }
        let mut bytes = String::new();
        for b in text.as_bytes() {
            match b {
                b' '..=b'~' if *b != b'"' && *b != b'\\' => bytes.push(*b as char),
                _ => {
                    let _ = write!(bytes, "\\{b:02X}");
                }
            }
        }
        let _ = writeln!(
            self.string_defs,
            "{g} = hidden constant [{} x i8] c\"{bytes}\\00\"",
            text.len() + 1
        );
        self.string_sizes.push((g.clone(), text.len() + 1));
        self.strings.insert(text.to_string(), g.clone());
        (g, text.len())
    }

    fn sym(&mut self, s: meadow_intern::InternedString) -> usize {
        match self.syms.iter().position(|x| *x == s) {
            Some(i) => i,
            None => {
                self.syms.push(s);
                self.syms.len() - 1
            }
        }
    }

    /// A **safe point**: where a thread that has had its turn gives way, so
    /// that one that never waits does not keep a core to itself. It stands at
    /// the entry of every definition and every method, which is to say on
    /// every loop, since a loop in AxCut is a jump to one of them.
    ///
    /// The flag is a byte in the runtime, set by the scheduler's timer only
    /// while there is a thread waiting for a core, so the usual answer is a
    /// load and a branch not taken. A program that cannot spawn has it as a
    /// constant zero of its own instead, and LLVM folds all of this away: see
    /// [`Module::units`].
    /// The running thread's context, as the code in hand names it: every
    /// function of a program that can spawn takes it first, in a register of
    /// its own, and hands it on in every call -- see [`CTX`]. A program that
    /// cannot keeps it in a static, and the runtime finds it there; `0` says
    /// so.
    fn cx(&self) -> &'static str {
        if self.threaded { CTX } else { "0" }
    }

    /// How many of a call's arguments travel in registers: all the
    /// convention has ([`CallConv::regs`]), less the one the context takes.
    fn regs(&self) -> usize {
        self.reg_words - usize::from(self.threaded)
    }

    /// The arguments of a call, the context first where there is one.
    fn arg_list(&self, ops: &[String]) -> String {
        let ctx = self.threaded.then(|| format!("i64 {CTX}"));
        ctx.into_iter()
            .chain(ops.iter().take(self.regs()).map(|o| format!("i64 {o}")))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// What does not fit in registers, stored in the spill area: see
    /// [`spill`].
    fn spill(&self, f: &mut Fun, ops: &[String]) {
        spill(f, ops, self.regs(), self.threaded);
    }

    /// Under a growable stack, where a function begins: if the stack pointer
    /// is below the running stack's limit (`silo/src/stack.rs`), the function
    /// is run again on a new piece of stack, through its trampoline, and what
    /// that answers is what it answers. The trampoline takes the context and
    /// the arguments from a buffer, and calls the function as any call does.
    fn stack_point(&mut self, f: &mut Fun) {
        if !self.growable {
            return;
        }
        let (grow, on) = (f.b(), f.b());
        // The stack pointer itself: under `ghccc` the frame pointer's
        // register carries an argument, so `llvm.frameaddress` is not it.
        let sp = f.t();
        f.i(format!("{sp} = call ptr @llvm.stacksave()"));
        let at = f.t();
        f.i(format!("{at} = ptrtoint ptr {sp} to i64"));
        let slot = if self.threaded {
            let c = f.ptr(CTX);
            let s = f.t();
            f.i(format!("{s} = getelementptr i64, ptr {c}, i64 2"));
            s
        } else {
            "@meadow_stack_limit_single".to_string()
        };
        let limit = f.t();
        f.i(format!("{limit} = load i64, ptr {slot}"));
        let low = f.t();
        f.i(format!("{low} = icmp ult i64 {at}, {limit}"));
        f.i(format!("br i1 {low}, label %{grow}, label %{on}, !prof !1"));
        f.label(&grow);
        let ctx = usize::from(self.threaded);
        let words = ctx + f.params.len();
        let buf = f.t();
        f.alloca(format!("{buf} = alloca [{} x i64]", words.max(1)));
        let store = |f: &mut Fun, i: usize, v: &str| {
            let q = f.t();
            f.i(format!("{q} = getelementptr i64, ptr {buf}, i64 {i}"));
            f.i(format!("store i64 {v}, ptr {q}"));
        };
        if self.threaded {
            store(f, 0, CTX);
        }
        for (i, p) in f.params.clone().iter().enumerate() {
            store(f, ctx + i, p);
        }
        let tramp = format!("@grow.{}", &f.name[1..]);
        let r = f.t();
        f.i(format!(
            "{r} = call i64 @meadow_stack_grow(ptr {tramp}, ptr {buf})"
        ));
        f.i(format!("ret i64 {r}"));
        f.label(&on);
        // The trampoline: the arguments back out of the buffer, and the call.
        let mut t = Fun::new(tramp.clone(), Vec::new());
        let load = |t: &mut Fun, i: usize, into: &str| {
            let q = t.t();
            t.i(format!("{q} = getelementptr i64, ptr %args, i64 {i}"));
            t.i(format!("{into} = load i64, ptr {q}"));
        };
        if self.threaded {
            load(&mut t, 0, CTX);
        }
        let args: Vec<String> = (0..f.params.len()).map(|i| format!("%arg{i}")).collect();
        for (i, a) in args.iter().enumerate() {
            load(&mut t, ctx + i, a);
        }
        self.spill(&mut t, &args);
        self.spill_words = self.spill_words.max(t.spill);
        let r = t.t();
        t.i(format!(
            "{r} = call {} i64 {}({})",
            self.cc,
            f.name,
            self.arg_list(&args)
        ));
        t.i(format!("ret i64 {r}"));
        self.trampoline = Some(format!(
            "define internal i64 {tramp}(ptr %args) noinline {{\nentry:\n{}{}}}\n\n",
            t.allocas, t.body
        ));
    }

    /// The runtime's entry a block is allocated through: counting's own, as it
    /// always was, or tracing's, so that neither asks which it is.
    fn acquire_fn(&self) -> &'static str {
        if self.tracing {
            "meadow_acquire_traced"
        } else {
            "meadow_acquire"
        }
    }

    fn safe_point(&self, f: &mut Fun) {
        // A program that cannot spawn has nothing to give way to.
        if !self.threaded {
            return;
        }
        let (give, on) = (f.b(), f.b());
        let flag = f.t();
        f.i(format!("{flag} = load volatile i8, ptr @meadow_preempt"));
        let c = f.t();
        f.i(format!("{c} = icmp ne i8 {flag}, 0"));
        f.i(format!("br i1 {c}, label %{give}, label %{on}"));
        f.label(&give);
        f.i("call void @meadow_preempted()".to_string());
        f.i(format!("br label %{on}"));
        f.label(&on);
    }

    /// Under tracing, where a function begins: if a collection is due, the
    /// collector is run here, with `held` -- the function's parameters, each
    /// with its descriptor -- in a record on the chain, so that everything the
    /// program holds is in one: the callers' frames are, from the calls they
    /// are waiting on, and the heap is reached from them. Nothing moves, so
    /// nothing is read back. A loop is a tail call, so every loop passes one.
    /// Answers `held`'s values as the code after it is to use them: as they
    /// were, unless blocks move, when what a collection left there is read
    /// back.
    fn collect_point(&self, f: &mut Fun, held: &[(String, D)]) -> Vec<String> {
        let mut now: Vec<String> = held.iter().map(|(v, _)| v.clone()).collect();
        if !self.tracing {
            return now;
        }
        let (run, on) = (f.b(), f.b());
        let flag = f.t();
        f.i(format!("{flag} = load volatile i8, ptr @meadow_gc_due"));
        let c = f.t();
        f.i(format!("{c} = icmp ne i8 {flag}, 0"));
        let from = f.cur.clone();
        f.i(format!("br i1 {c}, label %{run}, label %{on}, !prof !1"));
        f.label(&run);
        let mut which = Vec::new();
        let refs: Vec<(String, String)> = held
            .iter()
            .enumerate()
            .filter_map(|(i, (v, d))| match d {
                D::Known(k) if *k == desc::REF => Some((i, v.clone(), k.to_string())),
                D::Known(_) => None,
                D::Dyn(d) => Some((i, v.clone(), d.clone())),
            })
            .map(|(i, v, d)| {
                which.push(i);
                (v, d)
            })
            .collect();
        let words = 2 + 2 * refs.len();
        let rec = f.t();
        f.alloca(format!("{rec} = alloca [{words} x i64]"));
        let slot = if self.threaded {
            let c = f.ptr(CTX);
            let s = f.t();
            f.i(format!("{s} = getelementptr i64, ptr {c}, i64 1"));
            s
        } else {
            "@meadow_shadow_single".to_string()
        };
        let prev = f.t();
        f.i(format!("{prev} = load i64, ptr {slot}"));
        let store = |f: &mut Fun, i: usize, v: &str| {
            let q = f.t();
            f.i(format!("{q} = getelementptr i64, ptr {rec}, i64 {i}"));
            f.i(format!("store i64 {v}, ptr {q}"));
        };
        store(f, 0, &prev);
        store(f, 1, &refs.len().to_string());
        for (i, (v, d)) in refs.iter().enumerate() {
            store(f, 2 + 2 * i, v);
            store(f, 3 + 2 * i, d);
        }
        let at = f.t();
        f.i(format!("{at} = ptrtoint ptr {rec} to i64"));
        f.i(format!("store i64 {at}, ptr {slot}"));
        f.i(format!("call void @meadow_gc_poll(i64 {})", self.cx()));
        f.i(format!("store i64 {prev}, ptr {slot}"));
        // Where the collection left what the record held, if it moved it.
        let mut reloaded = Vec::new();
        if self.moving {
            for k in 0..which.len() {
                let (q, x) = (f.t(), f.t());
                f.i(format!(
                    "{q} = getelementptr i64, ptr {rec}, i64 {}",
                    2 + 2 * k
                ));
                f.i(format!("{x} = load i64, ptr {q}"));
                reloaded.push(x);
            }
        }
        let polled = f.cur.clone();
        f.i(format!("br label %{on}"));
        f.label(&on);
        for (k, i) in which.into_iter().enumerate().filter(|_| self.moving) {
            let x = f.t();
            f.i(format!(
                "{x} = phi i64 [ {}, %{from} ], [ {}, %{polled} ]",
                now[i], reloaded[k]
            ));
            now[i] = x;
        }
        now
    }

    /// What a unit declares of the runtime besides [`RUNTIME`]: the write
    /// barrier, where blocks move.
    fn remembering(&self) -> &'static str {
        if self.moving {
            "declare void @meadow_remember(i64, i64, i64)\n"
        } else {
            ""
        }
    }

    /// The write barrier: `v`, described by `d`, is stored in the block `into`,
    /// which may be older than it. Written only where blocks move.
    fn remember(&self, f: &mut Fun, into: &str, v: &str, d: &D) {
        if !self.moving {
            return;
        }
        let d = match d {
            D::Known(k) if *k == desc::REF => k.to_string(),
            D::Known(_) => return,
            D::Dyn(d) => d.clone(),
        };
        f.i(format!(
            "call void @meadow_remember(i64 {into}, i64 {v}, i64 {d})"
        ));
    }

    /// A definition: its function.
    /// Say which module the definitions written from here on are of: what
    /// [`Module::units`] divides the program by. A definition's methods and
    /// frames are written with it, and are of its module.
    pub fn in_module(&mut self, module: &str, def: &str) {
        if self.module != module {
            self.module = module.to_string();
        }
        if self.def_name != def {
            self.def_name = def.to_string();
        }
    }

    /// How much LLVM IR each definition came to, most first: its module, its
    /// name, the bytes of its functions and how many there are -- a
    /// definition's own, its methods' and its frames', and those of every
    /// copy of it a pass made. With `MEADOW_SIZES=N`, the `N` largest are
    /// said on stderr as the program is compiled: where a large build's IR
    /// comes from.
    pub fn sizes(&self) -> Vec<(String, String, usize, usize)> {
        let mut all: Vec<_> = self
            .sizes
            .iter()
            .map(|((m, d), (bytes, funs))| (m.clone(), d.clone(), *bytes, *funs))
            .collect();
        all.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| (&a.0, &a.1).cmp(&(&b.0, &b.1))));
        all
    }

    /// Name each label's function for the definition it is of, rather than
    /// by the label's number: `@mw.L.<module>.<name>.<k>`, the `k`th block of
    /// that name in its module, in the order given. A definition's methods
    /// and frames are named after it in turn.
    ///
    /// A label is a definition's place in the whole program, so a number in
    /// a name moved whenever anything before it did -- and with it the text
    /// of every unit that named the function, though nothing in them had
    /// changed. A name of its own moves only with its own module.
    pub fn name_labels(
        &mut self,
        labels: impl Iterator<Item = (meadow_axcut::Label, String, String)>,
    ) {
        let mut count: HashMap<String, usize> = HashMap::new();
        for (label, module, name) in labels {
            let stem = format!("{}.{}", symbol(&module), symbol(&name));
            let k = count.entry(stem.clone()).or_default();
            *k += 1;
            self.label_names
                .insert(label.0, format!("@mw.L.{stem}.{k}"));
        }
    }

    /// The function label `l` is: its name, or its number where it was given
    /// none.
    fn label_fn(&self, l: meadow_axcut::Label) -> String {
        self.label_names
            .get(&l.0)
            .cloned()
            .unwrap_or_else(|| format!("@mw.L{}", l.0))
    }

    pub fn def(&mut self, label: meadow_axcut::Label, b: &LBlock) -> Result<(), Error> {
        let params: Vec<String> = (0..b.params.len()).map(|i| format!("%a{i}")).collect();
        let name = self.label_fn(label);
        self.stem = name["@mw.L".len()..].to_string();
        self.stem_methods = 0;
        self.stem_frames = 0;
        let mut f = Fun::new(name, params.clone());
        self.stack_point(&mut f);
        self.safe_point(&mut f);
        let mut env = HashMap::new();
        for (n, p) in b.params.iter().zip(&params) {
            env.insert(*n, V::Val(p.clone()));
        }
        let held: Vec<(String, D)> = b
            .params
            .iter()
            .zip(&params)
            .map(|(n, p)| (p.clone(), self.desc(*n, &env)))
            .collect();
        let now = self.collect_point(&mut f, &held);
        for (n, v) in b.params.iter().zip(now) {
            env.insert(*n, V::Val(v));
        }
        self.stmt(&b.body, &mut env, &mut f)?;
        self.finish(f);
        self.drain()
    }

    /// Write the methods queued while writing functions, and theirs.
    fn drain(&mut self) -> Result<(), Error> {
        loop {
            if let Some((name, m, ncap)) = self.pending.pop() {
                self.method_fn(&name, &m, ncap)?;
            } else if let Some((name, fr)) = self.pending_frames.pop() {
                self.frame_body(&name, &fr)?;
            } else {
                return Ok(());
            }
        }
    }

    /// A frame's code, as a function of its captures and its argument:
    /// written once, and called wherever the frame is entered -- by the call
    /// it was made for returning, or by an `invoke` of it. Internal, so that
    /// LLVM inlines the ones entered from one place, within its own limits.
    ///
    /// Emitting the code at each place instead copied it there, and a frame
    /// entered from both arms of a branch -- a join -- inside another did so
    /// again: the standard library came out a gigabyte.
    ///
    /// A capture that is itself a frame not built is taken as *its* captures,
    /// and is a frame not built again inside: `f (g (h x))` returns into `g`'s
    /// continuation with `f`'s still waiting on the native stack, rather than
    /// building `f`'s as an object to hand across. Which captures are frames
    /// is fixed by where the frame was made, so it is the same at every entry.
    fn frame_body(&mut self, name: &str, fr: &Frame) -> Result<(), Error> {
        fn rebuild(caps: &[V], next: &mut usize) -> Vec<V> {
            caps.iter()
                .map(|c| match c {
                    V::Val(_) => {
                        *next += 1;
                        V::Val(format!("%a{}", *next - 1))
                    }
                    V::Frame(inner) => V::Frame(Rc::new(Frame {
                        id: inner.id,
                        captured: rebuild(&inner.captured, next),
                        method: inner.method.clone(),
                    })),
                })
                .collect()
        }
        let m = &fr.method;
        let mut next = 0;
        let captured = rebuild(&fr.captured, &mut next);
        let nargs = m.params.len() - fr.captured.len();
        let params: Vec<String> = (0..next + nargs).map(|i| format!("%a{i}")).collect();
        let mut f = Fun::new(name.to_string(), params.clone());
        let mut env = HashMap::new();
        for (n, v) in m.params.iter().zip(captured) {
            env.insert(*n, v);
        }
        for (n, p) in m.params[fr.captured.len()..].iter().zip(&params[next..]) {
            env.insert(*n, V::Val(p.clone()));
        }
        self.stmt(&m.body, &mut env, &mut f)?;
        self.finish_as(f, "hidden");
        Ok(())
    }

    /// The function a frame's code is: see [`Module::frame_body`].
    fn frame_fn(&mut self, fr: &Rc<Frame>) -> String {
        if let Some(n) = self.frame_fns.get(&fr.id) {
            return n.clone();
        }
        self.stem_frames += 1;
        let name = format!("@mw.F{}.{}", self.stem, self.stem_frames);
        self.frame_fns.insert(fr.id, name.clone());
        self.pending_frames.push((name.clone(), fr.clone()));
        name
    }

    /// What frame `fr` is entered with before its arguments: its captures,
    /// with a frame not built among them flattened into its own.
    fn flat_captures(caps: &[V], out: &mut Vec<String>) {
        for c in caps {
            match c {
                V::Val(v) => out.push(v.clone()),
                V::Frame(inner) => Self::flat_captures(&inner.captured, out),
            }
        }
    }

    /// Enter frame `fr` with `args` after its captures: a tail call to its
    /// code.
    fn call_frame(&mut self, fr: &Rc<Frame>, args: Vec<String>, f: &mut Fun) -> Result<(), Error> {
        let mut ops = Vec::with_capacity(fr.captured.len() + args.len());
        Self::flat_captures(&fr.captured, &mut ops);
        ops.extend(args);
        let callee = self.frame_fn(fr);
        self.spill(f, &ops);
        let list = self.arg_list(&ops);
        let r = f.t();
        f.i(format!("{r} = tail call {} i64 {callee}({list})", self.cc));
        f.i(format!("ret i64 {r}"));
        Ok(())
    }

    fn finish(&mut self, f: Fun) {
        self.finish_as(f, "hidden");
    }

    /// Every block's function is `noinline`: they tail-call one another, so
    /// a program is a few enormous strongly connected components of the call
    /// graph, which LLVM's inliner revisits until it has taken minutes and
    /// gigabytes on the standard library's tests. What is worth inlining --
    /// the counting helpers -- is `alwaysinline` instead.
    fn finish_as(&mut self, f: Fun, linkage: &str) {
        let regs = self.regs();
        self.spill_words = self
            .spill_words
            .max(f.spill)
            .max(f.params.len().saturating_sub(regs));
        let mut text = format!(
            "define {linkage} {} i64 {}({}) noinline {{\nentry:\n{}{}{}}}\n\n",
            self.cc,
            f.name,
            self.arg_list(&f.params),
            spilled_params(&f.params, regs, self.threaded),
            f.allocas,
            f.body
        );
        // In the same text, so in the same unit: it is internal to it.
        if let Some(t) = self.trampoline.take() {
            text.push_str(&t);
        }
        let words = f.params.len().min(regs) + usize::from(self.threaded);
        let size = self
            .sizes
            .entry((self.module.clone(), self.def_name.clone()))
            .or_default();
        size.0 += text.len();
        size.1 += 1;
        self.funs.push((f.name, text, words));
        self.fun_modules.push(self.module.clone());
    }

    /// A method's function: the object first, then the arguments. It loads
    /// its `ncap` captures from the object, releasing it.
    fn method_fn(&mut self, name: &str, m: &LBlock, ncap: usize) -> Result<(), Error> {
        let nargs = m.params.len() - ncap;
        let mut params = vec!["%obj".to_string()];
        params.extend((0..nargs).map(|i| format!("%a{i}")));
        let mut f = Fun::new(name.to_string(), params);
        self.stack_point(&mut f);
        self.safe_point(&mut f);
        let mut env = HashMap::new();
        let caps = &m.params[..ncap];
        let loaded = self.load_fields(&mut f, "%obj", ncap);
        self.release(
            &mut f,
            "%obj",
            caps,
            &loaded,
            &env_with(&env, caps, &loaded),
        )?;
        for (n, v) in caps.iter().zip(&loaded) {
            env.insert(*n, V::Val(v.clone()));
        }
        for (i, n) in m.params[ncap..].iter().enumerate() {
            env.insert(*n, V::Val(format!("%a{i}")));
        }
        // The object holds the captures, so it is what is kept of them --
        // unless blocks move, when what was loaded out of it must be found
        // where the collection left it too.
        let mut held = vec![("%obj".to_string(), D::Known(desc::REF))];
        let mut names = vec![None];
        if self.moving {
            for (n, v) in caps.iter().zip(&loaded) {
                held.push((v.clone(), self.desc(*n, &env)));
                names.push(Some(*n));
            }
        }
        for (i, n) in m.params[ncap..].iter().enumerate() {
            held.push((format!("%a{i}"), self.desc(*n, &env)));
            names.push(Some(*n));
        }
        let now = self.collect_point(&mut f, &held);
        for (n, v) in names.into_iter().zip(now) {
            if let Some(n) = n {
                env.insert(n, V::Val(v));
            }
        }
        self.stmt(&m.body, &mut env, &mut f)?;
        self.finish(f);
        Ok(())
    }

    /// Queue `methods` as functions, and answer the method-table index of the
    /// first.
    fn methods_for(&mut self, methods: &[LBlock], ncap: usize) -> usize {
        let base = self.methods.len();
        for m in methods {
            self.stem_methods += 1;
            let name = format!("@mw.M{}.{}", self.stem, self.stem_methods);
            self.next_method += 1;
            self.methods.push(name.clone());
            self.pending.push((name, m.clone(), ncap));
        }
        base
    }

    // --- descriptors and counting ----------------------------------------

    fn desc(&self, n: Name, env: &HashMap<Name, V>) -> D {
        // A reuse token is a block nobody counts -- its fields already taken
        // -- or nothing: never shared, and given back only by `Clean`, or by
        // erasing the frame that carries it (`erase_frame`). A frame that
        // carries one and is built as an object, then dropped unentered,
        // leaks the block: the one thing a token cannot be is erased as a
        // reference.
        if crate::linear::is_token(n) {
            return D::Known(desc::INT);
        }
        match self.program.reps.get(&n) {
            Some(Rep::Ref) | None => D::Known(desc::REF),
            Some(Rep::Int) => D::Known(desc::INT),
            Some(Rep::Float) => D::Known(desc::FLOAT),
            Some(Rep::Str) => D::Known(desc::STR),
            Some(Rep::Bits(d)) => D::Known(*d),
            Some(Rep::Var(d)) if *d == meadow_axcut::NO_DESC => D::Known(desc::ANY),
            Some(Rep::Var(d)) => match env.get(&VarId(*d)) {
                Some(V::Val(v)) => D::Dyn(v.clone()),
                _ => D::Known(desc::ANY),
            },
            Some(Rep::Unknown) => D::Known(desc::ANY),
        }
    }

    /// Share `v`, described by `d`, `times` times: a call to one of the
    /// module's helpers ([`helpers_text`]), which LLVM inlines where it judges it
    /// worth it -- the inline sequence at every one of the many places a
    /// linear program shares made the module several times the size.
    fn share(&self, f: &mut Fun, v: &str, d: &D, times: usize) {
        if self.tracing {
            return;
        }
        let c = count_ty(&self.layout);
        match d {
            D::Known(k) if *k == desc::REF => {
                f.i(format!("call void @mw.share(i64 {v}, {c} {times})"));
            }
            D::Known(_) => {}
            D::Dyn(r) => f.i(format!(
                "call void @mw.share_d(i64 {v}, i64 {r}, {c} {times})"
            )),
        }
    }

    /// Erase `v`, described by `d`: see [`Module::share`].
    fn erase(&self, f: &mut Fun, v: &str, d: &D) {
        if self.tracing {
            return;
        }
        match d {
            D::Known(k) if *k == desc::REF => {
                f.i(format!("call void @mw.erase(i64 {}, i64 {v})", self.cx()))
            }
            D::Known(_) => {}
            D::Dyn(r) => f.i(format!(
                "call void @mw.erase_d(i64 {}, i64 {v}, i64 {r})",
                self.cx()
            )),
        }
    }

    /// Give back a literal operand `k` of a `PrimK` or `BranchPrimK` once its
    /// primitive is done with it. It never had a name, so the ownership pass
    /// scheduled no erase; a string or big integer is a block made fresh for
    /// the one primitive, which only borrowed it. The others are words.
    fn erase_literal(&self, f: &mut Fun, k: &str, lit: &Lit) {
        self.erase(f, k, &D::Known(lit_desc(lit)));
    }

    /// Load `n` fields out of the non-uniform block `v`.
    fn load_fields(&self, f: &mut Fun, v: &str, n: usize) -> Vec<String> {
        if n == 0 {
            return Vec::new();
        }
        let first = self.layout.first_field(n, false);
        let p = f.ptr(v);
        (0..n)
            .map(|i| {
                let q = f.t();
                f.i(format!(
                    "{q} = getelementptr i64, ptr {p}, i64 {}",
                    first + i
                ));
                let x = f.t();
                f.i(format!("{x} = load i64, ptr {q}"));
                x
            })
            .collect()
    }

    /// The paper's `release`: `v`'s fields `loaded` (named `names`) are ours
    /// now. If `v` was the last reference the block goes back to the runtime
    /// clean, and no count moves; otherwise its count goes down and each field
    /// is shared.
    fn release(
        &self,
        f: &mut Fun,
        v: &str,
        names: &[Name],
        loaded: &[String],
        env: &HashMap<Name, V>,
    ) -> Result<(), Error> {
        // Traced: the fields are loaded, and nothing is counted.
        if self.tracing {
            return Ok(());
        }
        if names.is_empty() {
            // Nothing loaded: a nullary constructor, or a closure with no
            // captures -- which has no block, or one that is simply erased.
            self.erase(f, v, &D::Known(desc::REF));
            return Ok(());
        }
        let c = count_ty(&self.layout);
        let p = f.ptr(v);
        let rc = f.t();
        f.i(format!("{rc} = load {c}, ptr {p}"));
        let (clean, shared, done) = (f.b(), f.b(), f.b());
        let last = f.t();
        f.i(format!("{last} = icmp eq {c} {rc}, 0"));
        f.i(format!("br i1 {last}, label %{clean}, label %{shared}"));
        f.label(&clean);
        f.i(format!(
            "call void @meadow_clean(i64 {}, i64 {v})",
            self.cx()
        ));
        f.i(format!("br label %{done}"));
        f.label(&shared);
        let rc2 = f.t();
        f.i(format!("{rc2} = sub {c} {rc}, 1"));
        f.i(format!("store {c} {rc2}, ptr {p}"));
        for (n, x) in names.iter().zip(loaded) {
            let d = self.desc(*n, env);
            self.share(f, x, &d, 1);
        }
        self.region_released(f, v, &rc);
        f.i(format!("br label %{done}"));
        f.label(&done);
        Ok(())
    }

    /// The reference into a compact region that `v`, a block whose count was
    /// `rc`, held -- given up when `v` is released without being the last
    /// reference, if `v` is inside one: see `silo/src/region.rs`.
    ///
    /// Only after its fields are shared. Those are in the region too, and the
    /// reference `v` gives up may be the region's last: a value read out of a
    /// compact that is itself gone. Given up first, it would take the region
    /// with it, and the shares would write into memory already freed.
    fn region_released(&self, f: &mut Fun, v: &str, rc: &str) {
        if !self.regions {
            return;
        }
        let inreg = f.t();
        let (gone, on) = (f.b(), f.b());
        f.i(format!(
            "{inreg} = icmp ugt {} {rc}, {}",
            count_ty(&self.layout),
            self.layout.region_floor
        ));
        f.i(format!("br i1 {inreg}, label %{gone}, label %{on}"));
        f.label(&gone);
        f.i(format!("call void @meadow_region_erased(i64 {v})"));
        f.i(format!("br label %{on}"));
        f.label(&on);
    }

    /// [`Self::release`], keeping the block: the answer is a reuse token -- `v`
    /// itself when it was the last reference, its fields now ours and its
    /// memory free to build in, and `0` when it was shared, which is released
    /// as usual. See `linear`'s module docs.
    fn release_reuse(
        &self,
        f: &mut Fun,
        v: &str,
        meta: u64,
        names: &[Name],
        loaded: &[String],
        env: &HashMap<Name, V>,
    ) -> Result<String, Error> {
        // Traced: nobody knows whether anyone else holds the block, so
        // nothing is built in it.
        if self.tracing {
            return Ok("0".to_string());
        }
        let c = count_ty(&self.layout);
        let p = f.ptr(v);
        let rc = f.t();
        f.i(format!("{rc} = load {c}, ptr {p}"));
        let (mine, shared, shared_end, done) = (f.b(), f.b(), f.b(), f.b());
        let last = f.t();
        f.i(format!("{last} = icmp eq {c} {rc}, 0"));
        f.i(format!("br i1 {last}, label %{mine}, label %{shared}"));
        f.label(&mine);
        f.i(format!("br label %{done}"));
        f.label(&shared);
        let rc2 = f.t();
        f.i(format!("{rc2} = sub {c} {rc}, 1"));
        f.i(format!("store {c} {rc2}, ptr {p}"));
        for (n, x) in names.iter().zip(loaded) {
            let d = self.desc(*n, env);
            self.share(f, x, &d, 1);
        }
        self.region_released(f, v, &rc);
        f.i(format!("br label %{shared_end}"));
        f.label(&shared_end);
        f.i(format!("br label %{done}"));
        f.label(&done);
        let token = f.t();
        f.i(format!(
            "{token} = phi i64 [ {v}, %{mine} ], [ 0, %{shared_end} ]"
        ));
        f.origins.insert(
            token.clone(),
            Origin {
                meta,
                fields: loaded.to_vec(),
                descs: names.iter().map(|n| self.desc(*n, env)).collect(),
            },
        );
        Ok(token)
    }

    /// Erase `v`, keeping its block as a reuse token when this was the last
    /// reference: then its references to its fields, `names`, are given up
    /// -- the kept arm that loaded them holds its own -- and the block is the
    /// token; otherwise the token is `0`.
    fn drop_reuse(
        &self,
        f: &mut Fun,
        v: &str,
        names: &[Name],
        env: &HashMap<Name, V>,
    ) -> Result<String, Error> {
        if self.tracing {
            return Ok("0".to_string());
        }
        let c = count_ty(&self.layout);
        let p = f.ptr(v);
        let rc = f.t();
        f.i(format!("{rc} = load {c}, ptr {p}"));
        let (mine, shared, done) = (f.b(), f.b(), f.b());
        let last = f.t();
        f.i(format!("{last} = icmp eq {c} {rc}, 0"));
        f.i(format!("br i1 {last}, label %{mine}, label %{shared}"));
        f.label(&mine);
        let loaded = self.load_fields(f, v, names.len());
        for (n, x) in names.iter().zip(&loaded) {
            let d = self.desc(*n, env);
            self.erase(f, x, &d);
        }
        let mine_end = f.b();
        f.i(format!("br label %{mine_end}"));
        f.label(&mine_end);
        f.i(format!("br label %{done}"));
        f.label(&shared);
        self.erase(f, v, &D::Known(desc::REF));
        let shared_end = f.b();
        f.i(format!("br label %{shared_end}"));
        f.label(&shared_end);
        f.i(format!("br label %{done}"));
        f.label(&done);
        let token = f.t();
        f.i(format!(
            "{token} = phi i64 [ {v}, %{mine_end} ], [ 0, %{shared_end} ]"
        ));
        Ok(token)
    }

    /// Give reuse token `token` back unused: its block, if it holds one.
    fn clean_token(&self, f: &mut Fun, token: &str) {
        if self.tracing {
            return;
        }
        let (give, on) = (f.b(), f.b());
        let has = f.t();
        f.i(format!("{has} = icmp ne i64 {token}, 0"));
        f.i(format!("br i1 {has}, label %{give}, label %{on}"));
        f.label(&give);
        f.i(format!(
            "call void @meadow_clean(i64 {}, i64 {token})",
            self.cx()
        ));
        f.i(format!("br label %{on}"));
        f.label(&on);
    }

    /// A closure whose first method is `base` in the method table.
    ///
    /// Its `meta` is that index, which is the method's place among every
    /// method of the program -- so it is read from a constant the first unit
    /// defines ([`Module::method_index`]) rather than written here as a
    /// number, which would change this unit's text whenever a method was
    /// added anywhere before it.
    fn build_closure(&self, f: &mut Fun, base: usize, vals: &[String], descs: &[D]) -> String {
        let l = self.layout;
        let index = Self::method_index(&self.methods[base]);
        let m = f.t();
        f.i(format!("{m} = load i64, ptr {index}"));
        if vals.is_empty() {
            // A small value: `l.immediate(m)`, of an `m` read at run time.
            let s = f.t();
            f.i(format!("{s} = shl i64 {m}, {}", l.imm_shift));
            let v = f.t();
            f.i(format!("{v} = or i64 {s}, {}", l.imm_tag));
            return v;
        }
        let n = vals.len();
        let b = f.t();
        f.i(format!(
            "{b} = call i64 @{}(i64 {}, i64 {})",
            self.acquire_fn(),
            self.cx(),
            l.first_field(n, false) + n
        ));
        let closure = l.kinds.closure;
        if l.desc_at(0).0 == l.meta.word {
            // The method's index shares its word with descriptors: or-ed in
            // as the word is written.
            self.write_block_with(f, &b, closure, 0, Some(&m), vals, descs, None);
            return b;
        }
        // Written as method 0 -- the number itself must not be in the text --
        // and then again with what was read: LLVM drops the first store.
        self.write_block(f, &b, closure, 0, vals, descs, None);
        let p = f.ptr(&b);
        let w1 = f.t();
        f.i(format!(
            "{w1} = getelementptr i64, ptr {p}, i64 {}",
            l.meta.word
        ));
        let hi = f.t();
        f.i(format!("{hi} = shl i64 {m}, {}", l.meta.shift));
        let word = f.t();
        f.i(format!("{word} = or i64 {hi}, {}", l.kind.put(closure)));
        f.i(format!("store i64 {word}, ptr {w1}"));
        b
    }

    /// The constant holding the method-table index of the method function
    /// `name`: `@mw.M.x.1` has `@mw.mi.x.1`.
    fn method_index(name: &str) -> String {
        format!("@mw.mi{}", &name["@mw.M".len()..])
    }

    /// [`Self::build`], in reuse token `reuse`'s block when it holds one --
    /// the size of this one, since only a `let` of as many fields is given one
    /// -- and in a new block when it does not.
    ///
    /// A new block has every word written. A reused one has only what differs
    /// from what it held (Perceus's *reuse specialization*): its first word
    /// already says zero references and as many fields; its constructor and
    /// descriptors are left when they are the same; and a field is left when
    /// its new value is what was loaded out of that very slot -- the `l` and
    /// `r` of `Node c l k v r` rebuilt with a new colour, say. A token that
    /// came into this function as an argument has no known origin, and is
    /// written whole.
    fn build_in(
        &self,
        f: &mut Fun,
        kind: u64,
        meta: u64,
        vals: &[String],
        descs: &[D],
        reuse: Option<&str>,
    ) -> String {
        if vals.is_empty() {
            return format!("{}", self.layout.immediate(meta));
        }
        let n = vals.len();
        let words = self.layout.first_field(n, false) + n;
        match reuse {
            None => {
                let b = f.t();
                f.i(format!(
                    "{b} = call i64 @{}(i64 {}, i64 {words})",
                    self.acquire_fn(),
                    self.cx(),
                ));
                self.write_block(f, &b, kind, meta, vals, descs, None);
                b
            }
            Some(token) => {
                let origin = f.origins.get(token).cloned();
                let (keep, fresh, join) = (f.b(), f.b(), f.b());
                let has = f.t();
                f.i(format!("{has} = icmp ne i64 {token}, 0"));
                f.i(format!("br i1 {has}, label %{keep}, label %{fresh}"));
                f.label(&keep);
                self.write_block(f, token, kind, meta, vals, descs, Some(origin.as_ref()));
                let keep_end = f.b();
                f.i(format!("br label %{keep_end}"));
                f.label(&keep_end);
                f.i(format!("br label %{join}"));
                f.label(&fresh);
                let a = f.t();
                f.i(format!(
                    "{a} = call i64 @{}(i64 {}, i64 {words})",
                    self.acquire_fn(),
                    self.cx(),
                ));
                self.write_block(f, &a, kind, meta, vals, descs, None);
                let fresh_end = f.b();
                f.i(format!("br label %{fresh_end}"));
                f.label(&fresh_end);
                f.i(format!("br label %{join}"));
                f.label(&join);
                let b = f.t();
                f.i(format!(
                    "{b} = phi i64 [ {token}, %{keep_end} ], [ {a}, %{fresh_end} ]"
                ));
                b
            }
        }
    }

    /// Write block `b` as [`Self::build_in`] builds it. `reused` is `None`
    /// for a new block, and `Some` for a reuse token's -- with where the
    /// token came from, when that is known.
    #[allow(clippy::too_many_arguments)]
    fn write_block(
        &self,
        f: &mut Fun,
        b: &str,
        kind: u64,
        meta: u64,
        vals: &[String],
        descs: &[D],
        reused: Option<Option<&Origin>>,
    ) {
        self.write_block_with(f, b, kind, meta, None, vals, descs, reused)
    }

    /// [`Self::write_block`], with a `meta` read at run time, `dyn_meta`,
    /// or-ed into its word if there is one.
    ///
    /// Each word of the header and the descriptors is written once, in
    /// order: what is known of it as one number, and what is read at run
    /// time (a type variable's descriptor, the `meta`) or-ed in. A reused
    /// block's first word already says zero references and as many fields;
    /// its kind's word, and its descriptors' words, are left when they would
    /// be written as they are.
    #[allow(clippy::too_many_arguments)]
    fn write_block_with(
        &self,
        f: &mut Fun,
        b: &str,
        kind: u64,
        meta: u64,
        dyn_meta: Option<&str>,
        vals: &[String],
        descs: &[D],
        reused: Option<Option<&Origin>>,
    ) {
        let l = self.layout;
        let n = vals.len();
        let same = match reused {
            Some(Some(o)) if o.meta == meta && kind == l.kinds.data && o.fields.len() == n => {
                Some(o)
            }
            _ => None,
        };
        let same_descs = same.is_some_and(|o| {
            o.descs.as_slice() == descs && descs.iter().all(|d| matches!(d, D::Known(_)))
        });
        let p = f.ptr(b);
        let first = l.first_field(n, false);
        let mut known = vec![0u64; first];
        let mut dynamic: Vec<Vec<(u32, u64, String)>> = vec![Vec::new(); first];
        let mut described = vec![false; first];
        for (w, h) in l.header(kind, n as u64, meta).iter().enumerate() {
            known[w] |= h;
        }
        if let Some(m) = dyn_meta {
            dynamic[l.meta.word].push((l.meta.shift, l.meta.mask(), m.to_string()));
        }
        for (j, d) in descs.iter().enumerate() {
            let (w, shift) = l.desc_at(j);
            described[w] = true;
            match d {
                D::Known(k) => known[w] |= ((*k as u64) & l.desc_mask()) << shift,
                D::Dyn(r) => dynamic[w].push((shift, l.desc_mask(), r.clone())),
            }
        }
        for w in 0..first {
            let header = w < l.header_words;
            let kept = if header && w == l.len.word && w != l.kind.word && !described[w] {
                // The count and the length.
                reused.is_some()
            } else if header {
                same.is_some() && (!described[w] || same_descs)
            } else {
                same_descs
            };
            if kept {
                continue;
            }
            let mut acc = format!("{}", known[w]);
            for (shift, mask, r) in &dynamic[w] {
                let m = f.t();
                f.i(format!("{m} = and i64 {r}, {mask}"));
                let s = f.t();
                f.i(format!("{s} = shl i64 {m}, {shift}"));
                let o = f.t();
                f.i(format!("{o} = or i64 {acc}, {s}"));
                acc = o;
            }
            if w == 0 {
                f.i(format!("store i64 {acc}, ptr {p}"));
            } else {
                let q = f.t();
                f.i(format!("{q} = getelementptr i64, ptr {p}, i64 {w}"));
                f.i(format!("store i64 {acc}, ptr {q}"));
            }
        }
        let first = l.first_field(n, false);
        for (i, v) in vals.iter().enumerate() {
            if same.is_some_and(|o| o.fields[i] == *v) {
                continue;
            }
            let q = f.t();
            f.i(format!(
                "{q} = getelementptr i64, ptr {p}, i64 {}",
                first + i
            ));
            f.i(format!("store i64 {v}, ptr {q}"));
        }
    }

    // --- statements -------------------------------------------------------

    /// `n`'s value as an operand, building it if it is a frame not built.
    fn val(&mut self, n: Name, env: &mut HashMap<Name, V>, f: &mut Fun) -> Result<String, Error> {
        match env.get(&n).cloned() {
            Some(V::Val(v)) => Ok(v),
            Some(V::Frame(fr)) => {
                let v = self.materialize(&fr, env, f)?;
                env.insert(n, V::Val(v.clone()));
                Ok(v)
            }
            None => err(format!("{n:?} is not in scope in the native backend")),
        }
    }

    /// Build a frame as the object it would have been.
    fn materialize(
        &mut self,
        fr: &Frame,
        env: &mut HashMap<Name, V>,
        f: &mut Fun,
    ) -> Result<String, Error> {
        let ncap = fr.captured.len();
        // A frame that lends -- borrows what its block borrowed -- is only
        // ever entered natively (see `linear`): built as an object, it would
        // hold references it never took.
        if (0..ncap).any(|i| fr.method.borrows(i)) {
            return err("a frame that borrows what it holds was built as an object");
        }
        let base = match self.frame_tables.get(&fr.id) {
            Some(b) => *b,
            None => {
                let b = self.methods_for(std::slice::from_ref(&fr.method), ncap);
                self.frame_tables.insert(fr.id, b);
                b
            }
        };
        let mut vals = Vec::with_capacity(ncap);
        let mut descs = Vec::with_capacity(ncap);
        for (i, c) in fr.captured.iter().enumerate() {
            let v = match c {
                V::Val(v) => v.clone(),
                V::Frame(inner) => self.materialize(inner, env, f)?,
            };
            vals.push(v);
            descs.push(self.desc(fr.method.params[i], env));
        }
        Ok(self.build_closure(f, base, &vals, &descs))
    }

    fn stmt(&mut self, s: &L, env: &mut HashMap<Name, V>, f: &mut Fun) -> Result<(), Error> {
        match s {
            // Where it was written: for a debugger's line table, which native
            // code does not have yet.
            L::Mark(_, rest) => self.stmt(rest, env, f),
            L::Share(n, rest) => {
                let v = self.val(*n, env, f)?;
                let d = self.desc(*n, env);
                self.share(f, &v, &d, 1);
                self.stmt(rest, env, f)
            }
            L::Erase(n, rest) => {
                match env.get(n).cloned() {
                    // A frame nobody will call: what it captured goes.
                    Some(V::Frame(fr)) => self.erase_frame(&fr, env, f)?,
                    _ => {
                        let v = self.val(*n, env, f)?;
                        let d = self.desc(*n, env);
                        self.erase(f, &v, &d);
                    }
                }
                env.remove(n);
                self.stmt(rest, env, f)
            }
            L::Rename(binds, rest) => {
                let vals: Vec<(Name, V)> = binds
                    .iter()
                    .map(|(to, from)| {
                        env.get(from)
                            .cloned()
                            .map(|v| (*to, v))
                            .ok_or_else(|| Error {
                                msg: format!("{from:?} is not in scope in the native backend"),
                            })
                    })
                    .collect::<Result<_, _>>()?;
                for (to, v) in vals {
                    env.insert(to, v);
                }
                self.stmt(rest, env, f)
            }
            L::Let {
                name,
                tag,
                fields,
                reuse,
                rest,
            } => {
                let mut vals = Vec::with_capacity(fields.len());
                let mut descs = Vec::with_capacity(fields.len());
                for x in fields {
                    vals.push(self.val(*x, env, f)?);
                    descs.push(self.desc(*x, env));
                }
                let token = match reuse {
                    Some(t) => {
                        let tok = self.val(*t, env, f)?;
                        env.remove(t);
                        Some(tok)
                    }
                    None => None,
                };
                let data = self.layout.kinds.data;
                let v = self.build_in(f, data, *tag as u64, &vals, &descs, token.as_deref());
                env.insert(*name, V::Val(v));
                self.stmt(rest, env, f)
            }
            L::Clean(t, rest) => {
                let tok = self.val(*t, env, f)?;
                self.clean_token(f, &tok);
                env.remove(t);
                self.stmt(rest, env, f)
            }
            L::DropReuse {
                name,
                fields,
                token,
                rest,
            } => {
                let v = self.val(*name, env, f)?;
                let tok = self.drop_reuse(f, &v, fields, env)?;
                env.remove(name);
                env.insert(*token, V::Val(tok));
                self.stmt(rest, env, f)
            }
            L::Switch {
                scrutinee,
                arms,
                default,
                keep_default,
            } => self.switch(*scrutinee, arms, default, *keep_default, env, f),
            L::New {
                name,
                captures,
                methods,
                frame,
                rest,
            } => {
                let one_result =
                    methods.len() == 1 && methods[0].params.len() == captures.len() + 1;
                if *frame && one_result {
                    let captured = captures
                        .iter()
                        .map(|c| env.get(c).cloned())
                        .collect::<Option<Vec<V>>>()
                        .ok_or_else(|| Error {
                            msg: "a frame captures a name not in scope".into(),
                        })?;
                    env.insert(
                        *name,
                        V::Frame(Rc::new(Frame {
                            id: {
                                self.next_frame += 1;
                                self.next_frame
                            },
                            captured,
                            method: methods[0].clone(),
                        })),
                    );
                } else {
                    let base = self.methods_for(methods, captures.len());
                    let mut vals = Vec::with_capacity(captures.len());
                    let mut descs = Vec::with_capacity(captures.len());
                    for c in captures {
                        vals.push(self.val(*c, env, f)?);
                        descs.push(self.desc(*c, env));
                    }
                    let v = self.build_closure(f, base, &vals, &descs);
                    env.insert(*name, V::Val(v));
                }
                self.stmt(rest, env, f)
            }
            L::Jump { label, args } => {
                let callee = self.label_fn(*label);
                self.transfer(&callee, None, args, env, f)
            }
            L::Invoke { target, tag, args } => {
                if let Some(V::Frame(fr)) = env.get(target).cloned() {
                    // Our own frame, entered directly: its code, here.
                    return self.enter_frame(&fr, args, env, f);
                }
                let t = self.val(*target, env, f)?;
                // A continuation of one argument may be the null one: return
                // natively.
                if *tag == 0 && args.len() == 1 {
                    let a = self.val(args[0], env, f)?;
                    let (ret, call) = (f.b(), f.b());
                    let z = f.t();
                    f.i(format!("{z} = icmp eq i64 {t}, 0"));
                    f.i(format!("br i1 {z}, label %{ret}, label %{call}"));
                    f.label(&ret);
                    f.i(format!("ret i64 {a}"));
                    f.label(&call);
                }
                let fp = self.method_ptr(f, &t, *tag);
                self.transfer(&fp, Some(t), args, env, f)
            }
            L::Extern { op, args, blocks } => self.extern_op(op, args, blocks, env, f),
            L::Error(msg) => {
                let (g, len) = self.cstr(msg);
                f.i(format!("call void @meadow_fail(ptr {g}, i64 {len})"));
                f.i("unreachable".to_string());
                Ok(())
            }
        }
    }

    /// Link a record of what `fr` holds into the running segment's shadow
    /// chain, for the call it is about to wait on -- see
    /// `silo/src/shadow.rs`. Answers where the chain's head is and what it
    /// was, for putting back once the call returns; nothing when the program
    /// cannot abandon a continuation, or `fr` holds nothing to give up.
    fn shadow_push(
        &mut self,
        fr: &Frame,
        env: &HashMap<Name, V>,
        f: &mut Fun,
    ) -> Option<(String, String, String, usize)> {
        if !self.shadow {
            return None;
        }
        let mut held = Vec::new();
        self.shadow_entries(fr, env, &mut held);
        if held.is_empty() {
            return None;
        }
        let words = 2 + 2 * held.len();
        let rec = f.t();
        f.alloca(format!("{rec} = alloca [{words} x i64]"));
        let slot = if self.threaded {
            // Word 1 of the context: see `silo/src/ctx.rs`.
            let c = f.ptr(CTX);
            let s = f.t();
            f.i(format!("{s} = getelementptr i64, ptr {c}, i64 1"));
            s
        } else {
            "@meadow_shadow_single".to_string()
        };
        let prev = f.t();
        f.i(format!("{prev} = load i64, ptr {slot}"));
        let store = |f: &mut Fun, i: usize, v: &str| {
            let q = f.t();
            f.i(format!("{q} = getelementptr i64, ptr {rec}, i64 {i}"));
            f.i(format!("store i64 {v}, ptr {q}"));
        };
        store(f, 0, &prev);
        store(f, 1, &held.len().to_string());
        for (i, (v, d)) in held.iter().enumerate() {
            store(f, 2 + 2 * i, v);
            store(f, 3 + 2 * i, d);
        }
        let at = f.t();
        f.i(format!("{at} = ptrtoint ptr {rec} to i64"));
        f.i(format!("store i64 {at}, ptr {slot}"));
        Some((slot, prev, rec, held.len()))
    }

    /// What giving `fr` up would give up, as [`Module::erase_frame`] would:
    /// each value with its descriptor, or `TOKEN` for a reuse token. A value
    /// whose descriptor is known not to be a reference has nothing to give up.
    fn shadow_entries(&self, fr: &Frame, env: &HashMap<Name, V>, out: &mut Vec<(String, String)>) {
        for (i, c) in fr.captured.iter().enumerate() {
            // Borrowed: never the frame's to give up -- but where blocks move,
            // as much a reference to rewrite as any.
            if fr.method.borrows(i) && !self.moving {
                continue;
            }
            match c {
                V::Frame(inner) => self.shadow_entries(inner, env, out),
                V::Val(v) if crate::linear::is_token(fr.method.params[i]) => {
                    out.push((v.clone(), SHADOW_TOKEN.to_string()));
                }
                V::Val(v) => match self.capture_desc(fr, i, env) {
                    D::Known(k) if k == desc::REF => out.push((v.clone(), k.to_string())),
                    D::Known(_) => {}
                    D::Dyn(d) => out.push((v.clone(), d)),
                },
            }
        }
    }

    /// How the frame `fr`'s capture `i` is described: as its parameter is,
    /// where that is known -- but a value of a type variable's type is
    /// described by the descriptor that is another of the frame's captures,
    /// which is not a name of the code making the call: then, under tracing,
    /// by what the frame captured for it. Without that it is `ANY`, and a
    /// record would not hold the reference, which the collector then neither
    /// marks from the frame nor, where blocks move, rewrites.
    fn capture_desc(&self, fr: &Frame, i: usize, env: &HashMap<Name, V>) -> D {
        let n = fr.method.params[i];
        if self.tracing
            && let Some(Rep::Var(d)) = self.program.reps.get(&n)
            && *d != meadow_axcut::NO_DESC
            && !env.contains_key(&VarId(*d))
            && let Some(j) = fr.method.params.iter().position(|p| *p == VarId(*d))
            && let Some(V::Val(v)) = fr.captured.get(j)
        {
            return D::Dyn(v.clone());
        }
        self.desc(n, env)
    }

    /// `fr`, with what [`Module::shadow_entries`] recorded of it replaced by
    /// `vals`, in the same order: the frame to enter once a collection that
    /// moves blocks may have moved what it held.
    fn reframe(
        &self,
        fr: &Frame,
        env: &HashMap<Name, V>,
        vals: &mut impl Iterator<Item = String>,
    ) -> Frame {
        let captured = fr
            .captured
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if fr.method.borrows(i) && !self.moving {
                    return c.clone();
                }
                match c {
                    V::Frame(inner) => V::Frame(Rc::new(self.reframe(inner, env, vals))),
                    V::Val(_) if crate::linear::is_token(fr.method.params[i]) => {
                        V::Val(vals.next().expect("a token recorded"))
                    }
                    V::Val(_) => match self.capture_desc(fr, i, env) {
                        D::Known(k) if k == desc::REF => V::Val(vals.next().expect("recorded")),
                        D::Known(_) => c.clone(),
                        D::Dyn(_) => V::Val(vals.next().expect("recorded")),
                    },
                }
            })
            .collect();
        Frame {
            id: fr.id,
            captured,
            method: fr.method.clone(),
        }
    }

    fn erase_frame(
        &mut self,
        fr: &Frame,
        env: &mut HashMap<Name, V>,
        f: &mut Fun,
    ) -> Result<(), Error> {
        for (i, c) in fr.captured.iter().enumerate() {
            if fr.method.borrows(i) {
                continue;
            }
            match c {
                V::Frame(inner) => self.erase_frame(inner, env, f)?,
                V::Val(v) if crate::linear::is_token(fr.method.params[i]) => {
                    self.clean_token(f, v);
                }
                V::Val(v) => {
                    let d = self.desc(fr.method.params[i], env);
                    self.erase(f, v, &d);
                }
            }
        }
        Ok(())
    }

    /// A frame's method, emitted here with `args` after its captures.
    fn enter_frame(
        &mut self,
        fr: &Rc<Frame>,
        args: &[Name],
        env: &mut HashMap<Name, V>,
        f: &mut Fun,
    ) -> Result<(), Error> {
        let vals = args
            .iter()
            .map(|a| self.val(*a, env, f))
            .collect::<Result<Vec<_>, _>>()?;
        self.call_frame(fr, vals, f)
    }

    /// The function pointer of method `tag` of the object in `t`.
    fn method_ptr(&self, f: &mut Fun, t: &str, tag: u32) -> String {
        let m = self.meta_of(f, t);
        let idx = f.t();
        f.i(format!("{idx} = add i64 {m}, {tag}"));
        let slot = f.t();
        f.i(format!(
            "{slot} = getelementptr ptr, ptr @meadow_methods, i64 {idx}"
        ));
        let fp = f.t();
        f.i(format!("{fp} = load ptr, ptr {slot}"));
        fp
    }

    /// The `meta` of `v` -- a closure's first method, a constructor's tag --
    /// which a small value carries in itself and a block in its header.
    fn meta_of(&self, f: &mut Fun, v: &str) -> String {
        let l = self.layout;
        let (imm, blk, join) = (f.b(), f.b(), f.b());
        let low = f.t();
        f.i(format!("{low} = and i64 {v}, {}", l.imm_mask));
        let small = f.t();
        f.i(format!("{small} = icmp ne i64 {low}, 0"));
        f.i(format!("br i1 {small}, label %{imm}, label %{blk}"));
        f.label(&imm);
        let m1 = f.t();
        f.i(format!("{m1} = lshr i64 {v}, {}", l.imm_shift));
        f.i(format!("br label %{join}"));
        f.label(&blk);
        let p = f.ptr(v);
        let q = f.t();
        f.i(format!(
            "{q} = getelementptr i64, ptr {p}, i64 {}",
            l.meta.word
        ));
        let w1 = f.t();
        f.i(format!("{w1} = load i64, ptr {q}"));
        let m2 = read_field(f, &w1, l.meta);
        f.i(format!("br label %{join}"));
        f.label(&join);
        let m = f.t();
        f.i(format!("{m} = phi i64 [ {m1}, %{imm} ], [ {m2}, %{blk} ]"));
        m
    }

    /// Hand control to `callee` -- after `first`, the object of an invoke --
    /// with `args`. A tail call; unless one of the arguments is a frame not
    /// built, in which case it is a native call whose continuation is null,
    /// and the frame's code follows it with the answer.
    fn transfer(
        &mut self,
        callee: &str,
        first: Option<String>,
        args: &[Name],
        env: &mut HashMap<Name, V>,
        f: &mut Fun,
    ) -> Result<(), Error> {
        // The frame to return into natively: the last one among the
        // arguments. Any other is built.
        let native = args
            .iter()
            .rposition(|a| matches!(env.get(a), Some(V::Frame(_))));
        let mut ops: Vec<String> = first.into_iter().collect();
        let mut frame = None;
        for (i, a) in args.iter().enumerate() {
            if Some(i) == native {
                if let Some(V::Frame(fr)) = env.get(a).cloned() {
                    frame = Some(fr);
                }
                ops.push("0".into());
            } else {
                ops.push(self.val(*a, env, f)?);
            }
        }
        self.spill(f, &ops);
        let list = self.arg_list(&ops);
        match frame {
            None => {
                let r = f.t();
                f.i(format!("{r} = tail call {} i64 {callee}({list})", self.cc));
                f.i(format!("ret i64 {r}"));
                Ok(())
            }
            Some(fr) => {
                let pushed = self.shadow_push(&fr, env, f);
                let r = f.t();
                f.i(format!("{r} = call {} i64 {callee}({list})", self.cc));
                let mut fr = fr;
                if let Some((slot, prev, rec, n)) = pushed {
                    f.i(format!("store i64 {prev}, ptr {slot}"));
                    // What the frame held, where a collection during the call
                    // left it.
                    if self.moving {
                        let mut vals = Vec::with_capacity(n);
                        for k in 0..n {
                            let (q, x) = (f.t(), f.t());
                            f.i(format!(
                                "{q} = getelementptr i64, ptr {rec}, i64 {}",
                                2 + 2 * k
                            ));
                            f.i(format!("{x} = load i64, ptr {q}"));
                            vals.push(x);
                        }
                        fr = Rc::new(self.reframe(&fr, env, &mut vals.into_iter()));
                    }
                }
                self.call_frame(&fr, vec![r], f)
            }
        }
    }

    fn switch(
        &mut self,
        scrutinee: Name,
        arms: &[SwitchArm],
        default: &L,
        keep_default: bool,
        env: &mut HashMap<Name, V>,
        f: &mut Fun,
    ) -> Result<(), Error> {
        let s = self.val(scrutinee, env, f)?;
        let tag = self.meta_of(f, &s);
        let def = f.b();
        let labels: Vec<String> = arms.iter().map(|_| f.b()).collect();
        let mut sw = format!("switch i64 {tag}, label %{def} [");
        let mut seen = std::collections::HashSet::new();
        for (a, l) in arms.iter().zip(&labels) {
            if seen.insert(a.tag) {
                let _ = write!(sw, " i64 {}, label %{l}", a.tag);
            }
        }
        sw.push_str(" ]");
        f.i(sw);
        for (a, l) in arms.iter().zip(&labels) {
            f.label(l);
            let mut inner = env.clone();
            let loaded = self.load_fields(f, &s, a.fields.len());
            for (n, x) in a.fields.iter().zip(&loaded) {
                inner.insert(*n, V::Val(x.clone()));
            }
            // An arm that keeps the scrutinee borrows its fields: its body
            // shares what it uses of them, and nothing here is given up.
            match (a.keep, a.reuse) {
                (true, _) => {}
                (false, Some(t)) => {
                    let token =
                        self.release_reuse(f, &s, a.tag as u64, &a.fields, &loaded, &inner)?;
                    inner.insert(t, V::Val(token));
                }
                (false, None) => self.release(f, &s, &a.fields, &loaded, &inner)?,
            }
            self.stmt(&a.body, &mut inner, f)?;
        }
        f.label(&def);
        let mut inner = env.clone();
        if !keep_default {
            self.erase(f, &s, &D::Known(desc::REF));
        }
        self.stmt(default, &mut inner, f)
    }

    // --- primitives -------------------------------------------------------

    fn extern_op(
        &mut self,
        op: &Extern,
        args: &[Name],
        blocks: &[(Vec<Name>, L)],
        env: &mut HashMap<Name, V>,
        f: &mut Fun,
    ) -> Result<(), Error> {
        if op.is_branch() {
            let cond = match op {
                Extern::Branch => {
                    let v = self.val(args[0], env, f)?;
                    let c = f.t();
                    f.i(format!("{c} = icmp ne i64 {v}, 0"));
                    c
                }
                Extern::BranchPrim(p) => {
                    let (a, b) = (self.val(args[0], env, f)?, self.val(args[1], env, f)?);
                    let r = self.prim2(*p, &a, &b, args[0], env, f)?;
                    let c = f.t();
                    f.i(format!("{c} = icmp ne i64 {r}, 0"));
                    c
                }
                Extern::BranchPrimK(p, lit) => {
                    let a = self.val(args[0], env, f)?;
                    let k = self.literal(lit, f)?;
                    let r = self.prim2(*p, &a, &k, args[0], env, f)?;
                    self.erase_literal(f, &k, lit);
                    let c = f.t();
                    f.i(format!("{c} = icmp ne i64 {r}, 0"));
                    c
                }
                _ => unreachable!("is_branch"),
            };
            let [(_, on_false), (_, on_true)] = blocks else {
                return err("a branch needs two continuations");
            };
            let (t, e) = (f.b(), f.b());
            f.i(format!("br i1 {cond}, label %{t}, label %{e}"));
            f.label(&t);
            self.stmt(on_true, &mut env.clone(), f)?;
            f.label(&e);
            return self.stmt(on_false, &mut env.clone(), f);
        }
        let [(results, body)] = blocks else {
            return err("a primitive that answers needs one continuation");
        };
        // The stack-segment primitives take the code after them: see
        // `silo/src/segments.rs`. It is packed as a closure of one argument
        // -- what it binds -- and the runtime answers what that code finally
        // returns natively, which this returns.
        if let Extern::Prim(p @ (Prim::Enter | Prim::Detach | Prim::Reattach)) = op {
            let caps: Vec<Name> = crate::linear::free(body)
                .into_iter()
                .filter(|n| !results.contains(n))
                .collect();
            let mut params = caps.clone();
            params.extend(results.iter().copied());
            // It owns all it captures: see `linear`, which shared what the
            // code borrowed before packing it.
            let method = LBlock {
                params,
                borrowed: Vec::new(),
                body: body.clone(),
            };
            let base = self.methods_for(std::slice::from_ref(&method), caps.len());
            let mut vals = Vec::with_capacity(caps.len());
            let mut descs = Vec::with_capacity(caps.len());
            for c in &caps {
                vals.push(self.val(*c, env, f)?);
                descs.push(self.desc(*c, env));
            }
            let code = self.build_closure(f, base, &vals, &descs);
            let a = self.val(args[0], env, f)?;
            let entry = match p {
                Prim::Enter => "meadow_enter",
                Prim::Detach => "meadow_detach",
                _ => "meadow_reattach",
            };
            let r = f.t();
            f.i(format!("{r} = call i64 @{entry}(i64 {a}, i64 {code})"));
            f.i(format!("ret i64 {r}"));
            return Ok(());
        }
        let v = match op {
            Extern::Lit(l) => self.literal(l, f)?,
            Extern::Prim(p) => {
                let vals = args
                    .iter()
                    .map(|a| self.val(*a, env, f))
                    .collect::<Result<Vec<_>, _>>()?;
                let result = results.first().copied();
                if let Some(r) = self.inline_prim(*p, args, &vals, result, env, f) {
                    let mut inner = env.clone();
                    if let Some(n) = result {
                        inner.insert(n, V::Val(r));
                    }
                    return self.stmt(body, &mut inner, f);
                }
                match vals.as_slice() {
                    [a, b] if inline2(*p, self.rep(args[0])) => {
                        self.prim2(*p, a, b, args[0], env, f)?
                    }
                    [a] if *p == Prim::Neg && self.rep(args[0]) == Some(Rep::Int) => {
                        let r = f.t();
                        f.i(format!("{r} = sub i64 0, {a}"));
                        r
                    }
                    // The few a program does millions of times have an entry
                    // of their own in the runtime, taking their arguments in
                    // registers: see `direct`.
                    _ if direct(*p).is_some() => {
                        let (name, descs) = direct(*p).expect("checked");
                        let last = args.len() - 1;
                        let mut ops = Vec::new();
                        for (i, (a, v)) in args.iter().zip(&vals).enumerate() {
                            ops.push(format!("i64 {v}"));
                            let wanted =
                                descs == Descs::Each || (descs == Descs::Last && i == last);
                            if wanted {
                                ops.push(format!(
                                    "i64 {}",
                                    match self.desc(*a, env) {
                                        D::Known(k) => k.to_string(),
                                        D::Dyn(x) => x,
                                    }
                                ));
                            }
                        }
                        let r = f.t();
                        f.i(format!("{r} = call i64 @{name}({})", ops.join(", ")));
                        r
                    }
                    // A thread's function answers what the thread does, and
                    // the runtime keeps that answer for `await`: it is told
                    // how it is represented, as the VM is.
                    _ if *p == Prim::ThreadSpawn => {
                        let answer = match result.and_then(|r| self.program.threads.get(&r)) {
                            Some(Rep::Var(d)) if *d != meadow_axcut::NO_DESC => {
                                match env.get(&VarId(*d)) {
                                    Some(V::Val(v)) => D::Dyn(v.clone()),
                                    // Not `ANY`: Silo would hand the answer
                                    // across as a word, and a block of the
                                    // thread's heap would outlive the heap.
                                    _ => {
                                        return err("the descriptor of what a spawned thread \
                                             answers is not in scope");
                                    }
                                }
                            }
                            Some(rep) => D::Known(rep.desc().unwrap_or(desc::ANY)),
                            None => D::Known(desc::ANY),
                        };
                        let a = match answer {
                            D::Known(k) => k.to_string(),
                            D::Dyn(x) => x.clone(),
                        };
                        self.generic(*p, args, &vals, &[(a, D::Known(desc::INT))], env, f)
                    }
                    _ => self.generic(*p, args, &vals, &[], env, f),
                }
            }
            Extern::PrimK(p, lit) => {
                let a = self.val(args[0], env, f)?;
                let k = self.literal(lit, f)?;
                let r = if inline2(*p, self.rep(args[0])) {
                    self.prim2(*p, &a, &k, args[0], env, f)?
                } else if matches!(p, Prim::SetRef) {
                    // `setRef r 0`, `setRef done True`: a counter reset, a
                    // flag set -- as inline as with any other value.
                    self.set_ref(f, &a, &k, &D::Known(lit_desc(lit)));
                    "0".to_string()
                } else {
                    let kd = D::Known(lit_desc(lit));
                    self.generic(*p, args, &[a], &[(k.clone(), kd)], env, f)
                };
                self.erase_literal(f, &k, lit);
                r
            }
            Extern::Field(i) => {
                let a = self.val(args[0], env, f)?;
                let r = f.t();
                f.i(format!("{r} = call i64 @mw.field(i64 {a}, i64 {i})"));
                r
            }
            Extern::Array | Extern::Record(_) => {
                let vals = args
                    .iter()
                    .map(|a| self.val(*a, env, f))
                    .collect::<Result<Vec<_>, _>>()?;
                // A record keeps its labels in one order, the interned names'
                // -- which is decided here, in the compiler, as it is for the
                // other backends.
                let mut order: Vec<usize> = (0..args.len()).collect();
                let mut labels = Vec::new();
                if let Extern::Record(ls) = op {
                    order.sort_by_key(|i| ls[*i]);
                    labels = order.iter().map(|i| self.sym(ls[*i])).collect();
                }
                let n = args.len().max(1);
                let (av, dv) = (f.t(), f.t());
                f.alloca(format!("{av} = alloca [{n} x i64]"));
                f.alloca(format!("{dv} = alloca [{n} x i64]"));
                for (k, i) in order.iter().enumerate() {
                    let (q, r) = (f.t(), f.t());
                    f.i(format!("{q} = getelementptr i64, ptr {av}, i64 {k}"));
                    f.i(format!("store i64 {}, ptr {q}", vals[*i]));
                    f.i(format!("{r} = getelementptr i64, ptr {dv}, i64 {k}"));
                    let d = match self.desc(args[*i], env) {
                        D::Known(x) => x.to_string(),
                        D::Dyn(x) => x,
                    };
                    f.i(format!("store i64 {d}, ptr {r}"));
                }
                let r = f.t();
                if labels.is_empty() && matches!(op, Extern::Array) {
                    f.i(format!(
                        "{r} = call i64 @meadow_array(i64 {}, ptr {av}, ptr {dv})",
                        args.len()
                    ));
                } else {
                    let lv = f.t();
                    f.alloca(format!("{lv} = alloca [{n} x i64]"));
                    for (k, l) in labels.iter().enumerate() {
                        let q = f.t();
                        f.i(format!("{q} = getelementptr i64, ptr {lv}, i64 {k}"));
                        f.i(format!("store i64 {l}, ptr {q}"));
                    }
                    f.i(format!(
                        "{r} = call i64 @meadow_record(i64 {}, ptr {av}, ptr {dv}, ptr {lv})",
                        args.len()
                    ));
                }
                r
            }
            // A record keeps label and value in turn, in the labels' order: a
            // label whose place the type says has its value at `2 at + 1`.
            Extern::Select(_, Some(place)) => {
                let a = self.val(args[0], env, f)?;
                let r = f.t();
                let i = 2 * place.at + 1;
                f.i(format!("{r} = call i64 @mw.field(i64 {a}, i64 {i})"));
                r
            }
            Extern::Select(label, None) => {
                let a = self.val(args[0], env, f)?;
                let l = self.sym(*label);
                let r = f.t();
                f.i(format!("{r} = call i64 @meadow_select(i64 {a}, i64 {l})"));
                r
            }
            Extern::Extend(label, place) => {
                let rec = self.val(args[0], env, f)?;
                let v = self.val(args[1], env, f)?;
                let d = match self.desc(args[1], env) {
                    D::Known(x) => x.to_string(),
                    D::Dyn(x) => x,
                };
                let r = f.t();
                match place {
                    Some(p) => f.i(format!(
                        "{r} = call i64 @meadow_update(i64 {rec}, i64 {}, i64 {}, i64 {v}, i64 {d})",
                        p.at, p.of
                    )),
                    None => {
                        let l = self.sym(*label);
                        f.i(format!(
                            "{r} = call i64 @meadow_extend(i64 {rec}, i64 {l}, i64 {v}, i64 {d})"
                        ))
                    }
                }
                r
            }
            Extern::Native(effect, op) => {
                let a = self.val(args[0], env, f)?;
                let d = match self.desc(args[0], env) {
                    D::Known(k) => k.to_string(),
                    D::Dyn(x) => x,
                };
                let (ge, _) = self.cstr(effect);
                let (go, _) = self.cstr(op);
                let r = f.t();
                f.i(format!(
                    "{r} = call i64 @meadow_native(ptr {ge}, ptr {go}, i64 {a}, i64 {d})"
                ));
                r
            }
            other => {
                let (g, len) = self.cstr(&format!("the native backend cannot do {other:?} yet"));
                f.i(format!("call void @meadow_fail(ptr {g}, i64 {len})"));
                f.i("unreachable".into());
                return Ok(());
            }
        };
        let mut inner = env.clone();
        if let Some(r) = results.first() {
            inner.insert(*r, V::Val(v));
        }
        self.stmt(body, &mut inner, f)
    }

    fn rep(&self, n: Name) -> Option<Rep> {
        self.program.reps.get(&n).copied()
    }

    /// A literal's word. `Str` is a string, made by the runtime once and kept.
    fn literal(&mut self, l: &Lit, f: &mut Fun) -> Result<String, Error> {
        Ok(match l {
            Lit::Int(n) | Lit::AnyInt(n, _) => n.to_string(),
            Lit::Float(x) | Lit::AnyFloat(x, _) => (x.to_bits() as i64).to_string(),
            Lit::Float32(x) => (x.to_bits() as i64).to_string(),
            Lit::Word(_, bits) => (*bits as i64).to_string(),
            Lit::Char(c) => (*c as u32).to_string(),
            Lit::Bool(b) => u8::from(*b).to_string(),
            Lit::Unit => "0".into(),
            Lit::Sym(s) => self.sym(*s).to_string(),
            Lit::Str(s) => {
                let (g, len) = self.cstr(s);
                let r = f.t();
                f.i(format!("{r} = call i64 @meadow_text(ptr {g}, i64 {len})"));
                r
            }
            Lit::BigInt(n) => {
                let r = f.t();
                f.i(format!("{r} = call i64 @meadow_bigint(i64 {n})"));
                r
            }
        })
    }

    /// A binary primitive done inline: see [`inline2`] for which.
    fn prim2(
        &mut self,
        p: Prim,
        a: &str,
        b: &str,
        left: Name,
        env: &mut HashMap<Name, V>,
        f: &mut Fun,
    ) -> Result<String, Error> {
        let rep = self.rep(left);
        if !inline2(p, rep) {
            let d = self.desc(left, env);
            return Ok(self.generic(
                p,
                &[],
                &[],
                &[(a.to_string(), d.clone()), (b.to_string(), d)],
                env,
                f,
            ));
        }
        let float = rep == Some(Rep::Float);
        let p = untyped(p);
        let r = f.t();
        let bool_of = |f: &mut Fun, c: String| {
            let z = f.t();
            f.i(format!("{z} = zext i1 {c} to i64"));
            z
        };
        match p {
            Prim::Add => f.i(format!("{r} = add i64 {a}, {b}")),
            Prim::Sub => f.i(format!("{r} = sub i64 {a}, {b}")),
            Prim::Mul => f.i(format!("{r} = mul i64 {a}, {b}")),
            Prim::BitAnd => f.i(format!("{r} = and i64 {a}, {b}")),
            Prim::BitOr => f.i(format!("{r} = or i64 {a}, {b}")),
            Prim::BitXor => f.i(format!("{r} = xor i64 {a}, {b}")),
            Prim::Shl | Prim::Shr => {
                let m = f.t();
                f.i(format!("{m} = and i64 {b}, 63"));
                let ins = if p == Prim::Shl { "shl" } else { "ashr" };
                f.i(format!("{r} = {ins} i64 {a}, {m}"));
            }
            Prim::Div | Prim::Mod => return Ok(self.divide(p, a, b, f)),
            Prim::AddF | Prim::SubF | Prim::MulF | Prim::DivF => {
                let ins = match p {
                    Prim::AddF => "fadd",
                    Prim::SubF => "fsub",
                    Prim::MulF => "fmul",
                    _ => "fdiv",
                };
                let (x, y) = (f.t(), f.t());
                f.i(format!("{x} = bitcast i64 {a} to double"));
                f.i(format!("{y} = bitcast i64 {b} to double"));
                let z = f.t();
                f.i(format!("{z} = {ins} double {x}, {y}"));
                f.i(format!("{r} = bitcast double {z} to i64"));
            }
            Prim::Eq | Prim::Ne | Prim::Lt | Prim::Gt | Prim::Le | Prim::Ge => {
                let c = f.t();
                if float {
                    let (x, y) = (f.t(), f.t());
                    f.i(format!("{x} = bitcast i64 {a} to double"));
                    f.i(format!("{y} = bitcast i64 {b} to double"));
                    let pred = match p {
                        Prim::Eq => "oeq",
                        Prim::Ne => "une",
                        Prim::Lt => "olt",
                        Prim::Gt => "ogt",
                        Prim::Le => "ole",
                        _ => "oge",
                    };
                    f.i(format!("{c} = fcmp {pred} double {x}, {y}"));
                } else {
                    let pred = match p {
                        Prim::Eq => "eq",
                        Prim::Ne => "ne",
                        Prim::Lt => "slt",
                        Prim::Gt => "sgt",
                        Prim::Le => "sle",
                        _ => "sge",
                    };
                    f.i(format!("{c} = icmp {pred} i64 {a}, {b}"));
                }
                return Ok(bool_of(f, c));
            }
            Prim::LtF | Prim::GtF | Prim::LeF | Prim::GeF => {
                let (x, y) = (f.t(), f.t());
                f.i(format!("{x} = bitcast i64 {a} to double"));
                f.i(format!("{y} = bitcast i64 {b} to double"));
                let pred = match p {
                    Prim::LtF => "olt",
                    Prim::GtF => "ogt",
                    Prim::LeF => "ole",
                    _ => "oge",
                };
                let c = f.t();
                f.i(format!("{c} = fcmp {pred} double {x}, {y}"));
                return Ok(bool_of(f, c));
            }
            _ => return err(format!("{p:?} is not a primitive done inline")),
        }
        Ok(r)
    }

    /// The primitives done inline because they are words and loads: lengths,
    /// bytes, characters' codes, elements. A case the inline code does not
    /// cover -- an index out of bounds, a code past the surrogates -- calls
    /// the runtime, which says what is wrong as it always has.
    /// `setRef r v`, where `d` describes `v`: a `Ref` is a block of one field,
    /// its value, and the descriptor saying how to read it.
    fn set_ref(&self, f: &mut Fun, r: &str, v: &str, d: &D) {
        let l = self.layout;
        let p = f.ptr(r);
        let q = f.t();
        f.i(format!(
            "{q} = getelementptr i64, ptr {p}, i64 {}",
            l.first_field(1, false)
        ));
        let old = f.t();
        f.i(format!("{old} = load i64, ptr {q}"));
        // What it held is gone; what it holds now is one more reference. The
        // old one is erased by the same descriptor, since a `Ref` holds one
        // type.
        self.share(f, v, d, 1);
        self.remember(f, r, v, d);
        f.i(format!("store i64 {v}, ptr {q}"));
        self.erase(f, &old, d);
        if let D::Dyn(dv) = d {
            // A value of a type variable's type: the block says how to read
            // it, so say it.
            let (at, shift) = l.desc_at(0);
            let w = f.t();
            f.i(format!("{w} = getelementptr i64, ptr {p}, i64 {at}"));
            let had = f.t();
            f.i(format!("{had} = load i64, ptr {w}"));
            let (cleared, bits) = (f.t(), f.t());
            f.i(format!(
                "{cleared} = and i64 {had}, {}",
                !(l.desc_mask() << shift) as i64
            ));
            f.i(format!("{bits} = and i64 {dv}, {}", l.desc_mask()));
            let bits = if shift > 0 {
                let s = f.t();
                f.i(format!("{s} = shl i64 {bits}, {shift}"));
                s
            } else {
                bits
            };
            let now = f.t();
            f.i(format!("{now} = or i64 {cleared}, {bits}"));
            f.i(format!("store i64 {now}, ptr {w}"));
        }
    }

    fn inline_prim(
        &mut self,
        p: Prim,
        args: &[Name],
        vals: &[String],
        result: Option<Name>,
        env: &HashMap<Name, V>,
        f: &mut Fun,
    ) -> Option<String> {
        use Prim::*;
        let l = self.layout;
        // A header field of the block at `v`: its length, its `meta` -- which
        // for a string is how many bytes it has.
        let header = move |f: &mut Fun, v: &str, field: Field| {
            let p = f.ptr(v);
            let q = f.t();
            f.i(format!(
                "{q} = getelementptr i64, ptr {p}, i64 {}",
                field.word
            ));
            let w = f.t();
            f.i(format!("{w} = load i64, ptr {q}"));
            read_field(f, &w, field)
        };
        match (p, vals) {
            (ArrayLen | StArrayLen, [a]) => Some(header(f, a, l.len)),
            (StringByteLength, [s]) => Some(header(f, s, l.meta)),
            (CharCode, [c]) if self.rep(args[0]) == Some(Rep::Bits(desc::CHAR)) => Some(c.clone()),
            (ToInt, [x]) if self.rep(args[0]) == Some(Rep::Int) => Some(x.clone()),
            (ToInt, [x])
                if matches!(self.rep(args[0]), Some(Rep::Bits(d))
                    if (desc::WORD + 3..desc::WORD + 6).contains(&d)) =>
            {
                // `UInt8`, `UInt16`, `UInt32`: the word is the value.
                Some(x.clone())
            }
            (CharFromCode, [n]) if self.rep(args[0]) == Some(Rep::Int) => {
                Some(self.guarded(p, args, vals, env, f, |f| {
                    let c = f.t();
                    f.i(format!("{c} = icmp ult i64 {n}, 55296"));
                    (c, n.clone())
                }))
            }
            (StringByteAt, [s, i]) if self.rep(args[1]) == Some(Rep::Int) => {
                let (s, i) = (s.clone(), i.clone());
                Some(self.guarded(p, args, vals, env, f, move |f| {
                    let n = header(f, &s, l.meta);
                    let c = f.t();
                    f.i(format!("{c} = icmp ult i64 {i}, {n}"));
                    // Only read past the check: computed on the fast path.
                    (c, format!("byte:{s}:{i}"))
                }))
            }
            (ArrayGet | StGetArray, [a, i]) if self.rep(args[1]) == Some(Rep::Int) => {
                let d = result.map_or(D::Known(desc::REF), |r| self.desc(r, env));
                let (a, i) = (a.clone(), i.clone());
                Some(self.guarded(p, args, vals, env, f, move |f| {
                    let n = header(f, &a, l.len);
                    let c = f.t();
                    f.i(format!("{c} = icmp ult i64 {i}, {n}"));
                    (c, format!("elem:{a}:{i}"))
                }))
                .map(|x| {
                    self.share(f, &x, &d, 1);
                    x
                })
            }
            // An element that is not a reference: stored, with nothing to
            // count -- the old one needs no erasing, the new one no sharing.
            (StSetArray, [a, i, v])
                if self.rep(args[1]) == Some(Rep::Int)
                    && matches!(
                        self.rep(args[2]),
                        Some(Rep::Int | Rep::Float | Rep::Bits(_))
                    ) =>
            {
                let (a, i, v) = (a.clone(), i.clone(), v.clone());
                Some(self.guarded(p, args, vals, env, f, move |f| {
                    let n = header(f, &a, l.len);
                    let c = f.t();
                    f.i(format!("{c} = icmp ult i64 {i}, {n}"));
                    (c, format!("store:{a}:{i}:{v}"))
                }))
            }
            // A `Ref` is a block of one field: its value, with the descriptor
            // saying how to read it before it. Reading and writing one is what
            // a loop with a counter in it does at every turn, so neither is a
            // call.
            (GetRef, [r]) => {
                let d = result.map_or(D::Known(desc::REF), |n| self.desc(n, env));
                let p = f.ptr(r);
                let q = f.t();
                f.i(format!(
                    "{q} = getelementptr i64, ptr {p}, i64 {}",
                    l.first_field(1, false)
                ));
                let x = f.t();
                f.i(format!("{x} = load i64, ptr {q}"));
                self.share(f, &x, &d, 1);
                Some(x)
            }
            (SetRef, [r, v]) => {
                let d = self.desc(args[1], env);
                self.set_ref(f, r, v, &d);
                Some("0".to_string())
            }
            // A hole filled in place, as tail recursion modulo cons does at
            // every step: `meadow_prim` for it was a call, and a count of the
            // arguments, per cell built. The field is found past the
            // descriptor words, which a block's first word says how many of.
            // What the hole held -- the placeholder, of the field's own type
            // -- is erased by the value's descriptor, and the descriptor bits
            // already say what the value is; one not known here is left to
            // the runtime, which writes them. As `Layout::first_field` has it,
            // descriptors a header holds take no word past it.
            (SetField, [o, i, v])
                if self.rep(args[1]) == Some(Rep::Int)
                    && matches!(self.desc(args[2], env), D::Known(_)) =>
            {
                let d = self.desc(args[2], env);
                let p = f.ptr(o);
                // The length is in the word the block's address points at.
                let w0 = f.t();
                f.i(format!("{w0} = load i64, ptr {p}"));
                let n = read_field(f, &w0, l.len);
                let n = if l.inline_descs > 0 {
                    let (over, less, past) = (f.t(), f.t(), f.t());
                    let k = l.inline_descs;
                    f.i(format!("{over} = icmp ugt i64 {n}, {k}"));
                    f.i(format!("{less} = sub i64 {n}, {k}"));
                    f.i(format!("{past} = select i1 {over}, i64 {less}, i64 0"));
                    past
                } else {
                    n
                };
                let (up, dw, at, idx) = (f.t(), f.t(), f.t(), f.t());
                let per = l.descs_per_word;
                f.i(format!("{up} = add i64 {n}, {}", per - 1));
                f.i(format!("{dw} = lshr i64 {up}, {}", per.trailing_zeros()));
                f.i(format!("{at} = add i64 {dw}, {}", l.header_words));
                f.i(format!("{idx} = add i64 {at}, {i}"));
                let q = f.t();
                f.i(format!("{q} = getelementptr i64, ptr {p}, i64 {idx}"));
                let old = f.t();
                f.i(format!("{old} = load i64, ptr {q}"));
                self.share(f, v, &d, 1);
                self.remember(f, o, v, &d);
                f.i(format!("store i64 {v}, ptr {q}"));
                self.erase(f, &old, &d);
                Some("0".to_string())
            }
            (ToFloat, [x]) if self.rep(args[0]) == Some(Rep::Int) => {
                let d = f.t();
                f.i(format!("{d} = sitofp i64 {x} to double"));
                let r = f.t();
                f.i(format!("{r} = bitcast double {d} to i64"));
                Some(r)
            }
            _ => None,
        }
    }

    /// `check` (on the fast path) gives a condition and what to answer when it
    /// holds; otherwise the runtime does `p`. `byte:s:i` and `elem:a:i` in the
    /// answer mean a byte of a string or an element of an array, read only
    /// once the check has passed.
    fn guarded(
        &mut self,
        p: Prim,
        args: &[Name],
        vals: &[String],
        env: &HashMap<Name, V>,
        f: &mut Fun,
        check: impl FnOnce(&mut Fun) -> (String, String),
    ) -> String {
        // A string and an array are uniform: a string's bytes, and an array's
        // elements, start straight after the header.
        let first = self.layout.first_field(0, true);
        let (c, answer) = check(f);
        let (fast, slow, done) = (f.b(), f.b(), f.b());
        f.i(format!("br i1 {c}, label %{fast}, label %{slow}"));
        f.label(&fast);
        let quick = if let Some(rest) = answer.strip_prefix("byte:") {
            let (s, i) = rest.split_once(':').expect("byte:s:i");
            let p = f.ptr(s);
            let q = f.t();
            f.i(format!(
                "{q} = getelementptr i8, ptr {p}, i64 {}",
                8 * first
            ));
            let r = f.t();
            f.i(format!("{r} = getelementptr i8, ptr {q}, i64 {i}"));
            let b = f.t();
            f.i(format!("{b} = load i8, ptr {r}"));
            let x = f.t();
            f.i(format!("{x} = zext i8 {b} to i64"));
            x
        } else if let Some(rest) = answer.strip_prefix("elem:") {
            let (a, i) = rest.split_once(':').expect("elem:a:i");
            let k = f.t();
            f.i(format!("{k} = add i64 {i}, {first}"));
            let p = f.ptr(a);
            let q = f.t();
            f.i(format!("{q} = getelementptr i64, ptr {p}, i64 {k}"));
            let x = f.t();
            f.i(format!("{x} = load i64, ptr {q}"));
            x
        } else if let Some(rest) = answer.strip_prefix("store:") {
            let mut parts = rest.splitn(3, ':');
            let (a, i, v) = (
                parts.next().expect("store:a:i:v"),
                parts.next().expect("store:a:i:v"),
                parts.next().expect("store:a:i:v"),
            );
            let k = f.t();
            f.i(format!("{k} = add i64 {i}, {first}"));
            let p = f.ptr(a);
            let q = f.t();
            f.i(format!("{q} = getelementptr i64, ptr {p}, i64 {k}"));
            f.i(format!("store i64 {v}, ptr {q}"));
            "0".to_string()
        } else {
            answer
        };
        let from_fast = f.cur.clone();
        f.i(format!("br label %{done}"));
        f.label(&slow);
        // Only ever reached for what the check refused -- which, for an
        // element or a byte, the runtime reports and does not come back from,
        // so the share an element gets after this is the fast path's alone.
        let r = self.generic(p, args, vals, &[], env, f);
        let from_slow = f.cur.clone();
        f.i(format!("br label %{done}"));
        f.label(&done);
        let out = f.t();
        f.i(format!(
            "{out} = phi i64 [ {quick}, %{from_fast} ], [ {r}, %{from_slow} ]"
        ));
        out
    }

    /// `/` and `%` on `Int`s, as the VM does them: failing on zero, and
    /// wrapping `MIN / -1` -- which LLVM's `sdiv` would leave undefined.
    fn divide(&mut self, p: Prim, a: &str, b: &str, f: &mut Fun) -> String {
        let (zero, minus, normal, done) = (f.b(), f.b(), f.b(), f.b());
        let check = f.b();
        let z = f.t();
        f.i(format!("{z} = icmp eq i64 {b}, 0"));
        f.i(format!("br i1 {z}, label %{zero}, label %{check}"));
        f.label(&zero);
        let msg = if p == Prim::Div {
            "division by zero"
        } else {
            "modulo by zero"
        };
        let (g, len) = self.cstr(msg);
        f.i(format!("call void @meadow_fail(ptr {g}, i64 {len})"));
        f.i("unreachable".into());
        f.label(&check);
        let m = f.t();
        f.i(format!("{m} = icmp eq i64 {b}, -1"));
        f.i(format!("br i1 {m}, label %{minus}, label %{normal}"));
        f.label(&minus);
        let neg = f.t();
        if p == Prim::Div {
            f.i(format!("{neg} = sub i64 0, {a}"));
        } else {
            f.i(format!("{neg} = add i64 0, 0"));
        }
        f.i(format!("br label %{done}"));
        f.label(&normal);
        let q = f.t();
        let ins = if p == Prim::Div { "sdiv" } else { "srem" };
        f.i(format!("{q} = {ins} i64 {a}, {b}"));
        f.i(format!("br label %{done}"));
        f.label(&done);
        let r = f.t();
        f.i(format!(
            "{r} = phi i64 [ {neg}, %{minus} ], [ {q}, %{normal} ]"
        ));
        r
    }

    /// A primitive the runtime does: `@meadow_prim(code, n, args, descs)`,
    /// with the arguments in memory. `args`/`vals` are the named ones,
    /// `extra` any literal operands after them.
    fn generic(
        &mut self,
        p: Prim,
        args: &[Name],
        vals: &[String],
        extra: &[(String, D)],
        env: &HashMap<Name, V>,
        f: &mut Fun,
    ) -> String {
        let mut ops: Vec<(String, D)> = args
            .iter()
            .zip(vals)
            .map(|(a, v)| (v.clone(), self.desc(*a, env)))
            .collect();
        ops.extend(extra.iter().cloned());
        let n = ops.len().max(1);
        let (av, dv) = (f.t(), f.t());
        f.alloca(format!("{av} = alloca [{n} x i64]"));
        f.alloca(format!("{dv} = alloca [{n} x i64]"));
        for (i, (v, d)) in ops.iter().enumerate() {
            let (q, r) = (f.t(), f.t());
            f.i(format!("{q} = getelementptr i64, ptr {av}, i64 {i}"));
            f.i(format!("store i64 {v}, ptr {q}"));
            f.i(format!("{r} = getelementptr i64, ptr {dv}, i64 {i}"));
            let dval = match d {
                D::Known(k) => k.to_string(),
                D::Dyn(x) => x.clone(),
            };
            f.i(format!("store i64 {dval}, ptr {r}"));
        }
        let out = f.t();
        f.i(format!(
            "{out} = call i64 @meadow_prim(i64 {}, i64 {}, ptr {av}, ptr {dv})",
            p.code(),
            ops.len()
        ));
        out
    }

    /// The whole module's text, around its functions: the method table, the
    /// runtime's declarations, the entry point and the program's tables.
    pub fn text(self, entry: meadow_axcut::Label, result: i64, fingerprint: &str) -> String {
        self.units(Entries::Main(entry), result, fingerprint, usize::MAX)
            .remove(0)
            .1
    }

    /// [`Module::text`], in modules of about `unit` bytes each -- see
    /// [`Module::units`].
    pub fn text_split(
        self,
        entry: meadow_axcut::Label,
        result: i64,
        fingerprint: &str,
        unit: usize,
    ) -> Vec<(String, String)> {
        self.units(Entries::Main(entry), result, fingerprint, unit)
    }

    /// [`Module::text_split`], for a library: a C function for each of
    /// `exports`, and no `main`.
    pub fn text_library(
        self,
        exports: &[meadow_axcut::Export],
        fingerprint: &str,
        unit: usize,
    ) -> Vec<(String, String)> {
        self.units(
            Entries::Library(exports.to_vec()),
            meadow_rt::desc::ANY,
            fingerprint,
            unit,
        )
    }

    /// [`Module::text_split`], for a test executable: a table of `entries`,
    /// and a `main` that runs the one its first argument numbers.
    pub fn text_tests(
        self,
        entries: &[meadow_axcut::Label],
        fingerprint: &str,
        unit: usize,
    ) -> Vec<(String, String)> {
        self.units(
            Entries::Tests(entries.to_vec()),
            meadow_rt::desc::ANY,
            fingerprint,
            unit,
        )
    }

    /// The program as LLVM modules, each with the name it is known by, to be
    /// compiled apart -- in parallel -- and linked: one for each module of
    /// the program, and more for a module with over `unit` bytes of
    /// functions. The first holds the tables and the entry points; each
    /// declares what it uses of the others.
    ///
    /// By module, as `rustc` divides a crate, so that a unit is the same code
    /// for as long as its module is: what a build that keeps its objects
    /// needs, and what makes a file's name say what is in it.
    ///
    /// One module is what LLVM handles worst: its interprocedural passes grow
    /// faster than the program, and a module of the standard library's tests
    /// took tens of gigabytes. Apart, each is quick, and they use every core.
    ///
    /// **A program that never spawns pays for none of it.** Whether it can is
    /// plain here: it spawns somewhere, or it never does. When it never does,
    /// every unit gets a constant zero for the preemption flag, which folds
    /// each safe point away, and a spill area of its own, which is an
    /// ordinary global rather than something the runtime has to look up per
    /// thread. The runtime reads `@meadow_threaded` and keeps its one
    /// thread's state in a static, with no scheduler and no thread-local at
    /// all.
    fn units(
        mut self,
        entries: Entries,
        result: i64,
        fingerprint: &str,
        unit: usize,
    ) -> Vec<(String, String)> {
        let header = self.header(entries, result, fingerprint);
        let funs = std::mem::take(&mut self.funs);
        let arity: HashMap<&str, usize> = funs.iter().map(|(n, _, r)| (n.as_str(), *r)).collect();
        // A module's functions are a unit of their own, or several where
        // there are more than `unit` bytes of them, in the order they were
        // written: a definition and its methods together, which call one
        // another most. What is of no module -- an entry point, a program a
        // test wrote as one text -- goes with the header, and past `unit`
        // into units known by their number.
        let modules = std::mem::take(&mut self.fun_modules);
        let mut order: Vec<&str> = vec![""];
        let mut of: HashMap<&str, Vec<&str>> = HashMap::new();
        for ((_, text, _), module) in funs.iter().zip(&modules) {
            // One unit, when that is what was asked for.
            let module = if unit == usize::MAX {
                ""
            } else {
                module.as_str()
            };
            let texts = of.entry(module).or_default();
            if texts.is_empty() && !module.is_empty() {
                order.push(module);
            }
            texts.push(text);
        }
        let mut chunks: Vec<(String, String)> = vec![(String::new(), header)];
        for module in order {
            let mut part = 1;
            let mut size = 0;
            if !module.is_empty() {
                chunks.push((module.to_string(), String::new()));
            }
            for text in of.get(module).into_iter().flatten() {
                if size >= unit {
                    part += 1;
                    let name = if module.is_empty() {
                        (part - 1).to_string()
                    } else {
                        format!("{module}.{part}")
                    };
                    chunks.push((name, String::new()));
                    size = 0;
                }
                size += text.len();
                chunks.last_mut().expect("one").1.push_str(text);
            }
        }
        // The method indexes a unit reads, declared as it first names them.
        let indexes_of = |body: &str| -> String {
            let mut out = String::new();
            let mut seen = std::collections::HashSet::new();
            let mut rest = body;
            while let Some(at) = rest.find("@mw.mi") {
                let tail = &rest[at..];
                let len = 6 + tail[6..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_'))
                    .unwrap_or(tail.len() - 6);
                let name = &tail[..len];
                rest = &tail[len..];
                if seen.insert(name) {
                    let _ = writeln!(out, "{name} = external hidden constant i64");
                }
            }
            out
        };
        let first_only = format!(
            "@meadow_threaded = constant i8 {}\n@meadow_cycles = constant i8 {}\n\
             @meadow_gc = constant i8 {}\n\
             @meadow_spill_words = constant i64 {}\n\n",
            u8::from(self.threaded),
            u8::from(self.cycles),
            if self.moving {
                2
            } else {
                u8::from(self.tracing)
            },
            self.spill_area()
        );
        let string_sizes: HashMap<&str, usize> = self
            .string_sizes
            .iter()
            .map(|(g, n)| (g.as_str(), *n))
            .collect();
        // The strings a unit names, declared in the order it first names
        // them: every string of the program, declared in every unit, made
        // each unit's text change with any string anywhere.
        let strings_of = |body: &str| -> String {
            let mut out = String::new();
            let mut seen = std::collections::HashSet::new();
            let mut rest = body;
            while let Some(at) = rest.find("@mw.s.") {
                let tail = &rest[at..];
                let len = 6 + tail[6..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(tail.len() - 6);
                let name = &tail[..len];
                rest = &tail[len..];
                if let Some(n) = string_sizes.get(name)
                    && seen.insert(name)
                {
                    let _ = writeln!(out, "{name} = external hidden constant [{n} x i8]");
                }
            }
            out
        };
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, (name, body))| {
                let mut out = String::new();
                if i > 0 {
                    let _ = writeln!(
                        out,
                        "; A Meadow program, compiled by meadow-llvm: part {i}.\n"
                    );
                    out.push_str(RUNTIME);
                    out.push_str(self.remembering());
                    out.push_str(&helpers(
                        &self.layout,
                        self.tracing,
                        self.cycles,
                        self.regions,
                    ));
                    // Of no stated length: how many methods the program has
                    // is not this unit's to say, and would be one more thing
                    // that changed its text when another unit changed.
                    out.push_str("@meadow_methods = external hidden constant [0 x ptr]\n");
                    out.push_str(&strings_of(&body));
                    out.push_str(&indexes_of(&body));
                    out.push('\n');
                }
                out.push_str(&self.threads_part(i == 0));
                if i == 0 {
                    out.push_str(&first_only);
                }
                out.push_str(&declarations(&body, &arity, self.cc));
                out.push_str(&body);
                (name, out)
            })
            .collect()
    }

    /// How many words the spill area has: enough for the widest call, and
    /// never fewer than it always had.
    fn spill_area(&self) -> usize {
        self.spill_words.max(256)
    }

    /// What each unit says about threads: see [`Module::units`]. `first` is
    /// the unit that defines what the others share.
    fn threads_part(&self, first: bool) -> String {
        let mut out = String::new();
        if self.tracing || self.growable {
            // The branch to a collection, or to a new piece of stack, is cold.
            out.push_str("!1 = !{!\"branch_weights\", i32 1, i32 65536}\n");
        }
        if self.growable {
            // See [`Module::stack_point`].
            out.push_str("declare ptr @llvm.stacksave()\n");
            out.push_str("declare i64 @meadow_stack_grow(ptr, ptr)\n");
            if !self.threaded {
                out.push_str("@meadow_stack_limit_single = external global i64\n");
            }
        }
        if self.tracing {
            // Set by the runtime when a heap wants collecting: see
            // [`Module::collect_point`].
            out.push_str("@meadow_gc_due = external global i8\n");
            if !self.threaded && !self.shadow {
                out.push_str("@meadow_shadow_single = external global i64\n");
            }
        }
        if self.threaded {
            // The context comes in as every function's first argument, and
            // the spill area and the shadow chain's head are in it; only the
            // ways in from the runtime ask for it (`meadow_ctx`).
            out.push_str("@meadow_preempt = external hidden global i8\n");
            out.push_str("declare i64 @meadow_ctx()\n");
        } else {
            if self.shadow {
                out.push_str("@meadow_shadow_single = external global i64\n");
            }
            // No `spawn` anywhere: nothing ever sets the flag, and the one
            // thread's spill area can be an ordinary global -- one, defined in
            // the first unit, since a call and the function it enters may be
            // in different units.
            out.push_str("@meadow_preempt = internal constant i8 0\n");
            let words = self.spill_area();
            if first {
                let _ = writeln!(
                    out,
                    "@mw.spill = hidden global [{words} x i64] zeroinitializer"
                );
            } else {
                let _ = writeln!(out, "@mw.spill = external hidden global [{words} x i64]");
            }
            out.push_str(
                "define internal ptr @meadow_spill_area() alwaysinline {\n  \
                 ret ptr @mw.spill\n}\n",
            );
        }
        out.push('\n');
        out
    }

    /// How a way in from the runtime finds the context to hand the code it
    /// calls, and the argument that hands it on: nothing, where there is none.
    fn entry_ctx(&self) -> (String, String) {
        if self.threaded {
            (
                format!("  {CTX} = call i64 @meadow_ctx()\n"),
                format!("i64 {CTX}, "),
            )
        } else {
            (String::new(), String::new())
        }
    }

    /// The first module's own part: the runtime's declarations, the tables,
    /// and the entry points.
    fn header(&mut self, entries: Entries, result: i64, fingerprint: &str) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "; A Meadow program, compiled by meadow-llvm.\n");
        out.push_str(RUNTIME);
        out.push_str(self.remembering());
        out.push_str(&helpers(
            &self.layout,
            self.tracing,
            self.cycles,
            self.regions,
        ));
        out.push_str(&invokers(&self.layout, self.cc, self.threaded));
        let symbol = crate::symbol_of(fingerprint, self.layout);
        let _ = writeln!(out, "@{symbol} = external global i8");
        let _ = writeln!(out, "@meadow_runtime = constant ptr @{symbol}\n");
        let methods: Vec<String> = self.methods.iter().map(|m| format!("ptr {m}")).collect();
        let _ = writeln!(
            out,
            "@meadow_methods = hidden constant {}\n",
            array(&methods)
        );
        // Each method's place in that table, for the code that builds a
        // closure to read: see [`Module::build_closure`].
        for (i, m) in self.methods.iter().enumerate() {
            let _ = writeln!(out, "{} = hidden constant i64 {i}", Self::method_index(m));
        }
        out.push('\n');
        // Constructor names by tag, for printing.
        let mut ctors: Vec<(u32, String)> = self
            .program
            .tags
            .iter()
            .map(|(name, tag)| (*tag, name.to_string()))
            .collect();
        ctors.sort();
        let count = ctors.last().map_or(0, |(t, _)| *t as usize + 1);
        let mut names = vec!["ptr null".to_string(); count];
        for (t, name) in &ctors {
            let (g, _) = self.cstr(name);
            names[*t as usize] = format!("ptr {g}");
        }
        let _ = writeln!(out, "@meadow_ctor_names = constant {}", array(&names));
        // And the named fields of each `record` constructor, for `.field` on one.
        let mut fields = vec!["ptr null".to_string(); count];
        for (t, name) in &ctors {
            if let Some(fs) = self
                .program
                .ctor_fields
                .get(&meadow_intern::InternedString::from(name.as_str()))
            {
                let joined = fs
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let (g, _) = self.cstr(&joined);
                fields[*t as usize] = format!("ptr {g}");
            }
        }
        let _ = writeln!(out, "@meadow_ctor_fields = constant {}", array(&fields));
        let _ = writeln!(out, "@meadow_ctor_count = constant i64 {count}");
        // The constructor playing each runtime role, in `Role::ALL`'s order,
        // which the runtime reads them in: see `meadow_rt::roles`.
        let roles: Vec<String> = meadow_rt::roles::Role::ALL
            .into_iter()
            .map(|role| match self.program.role(role) {
                Some(c) => format!("ptr {}", self.cstr(&c).0),
                None => "ptr null".to_string(),
            })
            .collect();
        let _ = writeln!(out, "@meadow_role_names = constant {}", array(&roles));
        let syms: Vec<String> = self.syms.clone().iter().map(|s| s.to_string()).collect();
        let mut sym_ptrs = Vec::new();
        for s in &syms {
            let (g, _) = self.cstr(s);
            sym_ptrs.push(format!("ptr {g}"));
        }
        let _ = writeln!(out, "@meadow_sym_names = constant {}", array(&sym_ptrs));
        let _ = writeln!(out, "@meadow_sym_count = constant i64 {}", syms.len());
        // Where each goes among a record's labels: the interned names' order.
        let mut by: Vec<usize> = (0..self.syms.len()).collect();
        by.sort_by_key(|i| self.syms[*i]);
        let mut ranks = vec![0usize; self.syms.len()];
        for (r, i) in by.iter().enumerate() {
            ranks[*i] = r;
        }
        let ranks: Vec<String> = ranks.iter().map(|r| format!("i64 {r}")).collect();
        let _ = writeln!(
            out,
            "@meadow_sym_ranks = constant {}",
            if ranks.is_empty() {
                "[0 x i64] zeroinitializer".to_string()
            } else {
                format!("[{} x i64] [{}]", ranks.len(), ranks.join(", "))
            }
        );
        let _ = writeln!(out, "@meadow_result_desc = constant i64 {result}\n");
        out.push_str(&self.string_defs);
        out.push('\n');
        match entries {
            Entries::Main(entry) => {
                let (get, arg) = self.entry_ctx();
                let _ = writeln!(
                    out,
                    "define i64 @meadow_entry() {{\n{get}  %r = call {} i64 {}({arg}i64 0)\n  ret i64 %r\n}}\n",
                    self.cc,
                    self.label_fn(entry)
                );
                out.push_str(
                    "define i32 @main(i32 %argc, ptr %argv) {\n  \
                     %r = call i32 @meadow_run(ptr @meadow_entry, i32 %argc, ptr %argv)\n  \
                     ret i32 %r\n}\n\n",
                );
            }
            Entries::Tests(labels) => {
                let mut table = Vec::new();
                let (get, arg) = self.entry_ctx();
                for (i, l) in labels.iter().enumerate() {
                    let _ = writeln!(
                        out,
                        "define i64 @meadow_test{i}() {{\n{get}  %r = call {} i64 {}({arg}i64 0)\n  ret i64 %r\n}}\n",
                        self.cc,
                        self.label_fn(*l)
                    );
                    table.push(format!("ptr @meadow_test{i}"));
                }
                let _ = writeln!(out, "@meadow_tests = constant {}\n", array(&table));
                let _ = writeln!(
                    out,
                    "define i32 @main(i32 %argc, ptr %argv) {{\n  \
                     %r = call i32 @meadow_run_test(ptr @meadow_tests, i64 {}, i32 %argc, ptr %argv)\n  \
                     ret i32 %r\n}}\n",
                    labels.len()
                );
            }
            Entries::Library(exports) => {
                let (get, arg) = self.entry_ctx();
                for e in &exports {
                    let callee = self.label_fn(e.label);
                    let cc = self.cc.to_string();
                    if let Err(why) = export_fn(
                        &mut out,
                        &e.name,
                        &callee,
                        &cc,
                        (&get, &arg),
                        &e.params,
                        e.result,
                    ) {
                        let _ = writeln!(out, "; {why}");
                    }
                }
            }
        }
        out
    }
}

/// `name` as it can stand in an LLVM symbol: letters, digits, `.` and `_`
/// as they are, and `_` for anything else.
fn symbol(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Declarations of the functions `body` calls and does not define, which
/// another unit does.
fn declarations(body: &str, arity: &HashMap<&str, usize>, cc: &str) -> String {
    let mut defined = std::collections::HashSet::new();
    let mut used = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in body.lines() {
        let def = line.starts_with("define ");
        let mut rest = line;
        while let Some(at) = rest.find("@mw.") {
            let tail = &rest[at..];
            let len = 4 + tail[4..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_'))
                .unwrap_or(tail.len() - 4);
            let name = &tail[..len];
            rest = &tail[len..];
            if !arity.contains_key(name) {
                continue;
            }
            if def {
                defined.insert(name);
                break;
            }
            if seen.insert(name) {
                used.push(name);
            }
        }
    }
    let mut out = String::new();
    for name in used {
        if !defined.contains(name) {
            let params = vec!["i64"; arity[name]].join(", ");
            let _ = writeln!(out, "declare hidden {cc} i64 {name}({params})");
        }
    }
    out.push('\n');
    out
}

/// What the emitted code calls in the runtime, `silo/src/lib.rs`.
const RUNTIME: &str = "\
declare i64 @meadow_acquire(i64, i64)
declare i64 @meadow_acquire_traced(i64, i64)
declare void @meadow_export_enter()
declare i32 @meadow_export_run(ptr, ptr)
declare void @meadow_free(i64, i64)
declare void @meadow_clean(i64, i64)
declare void @meadow_fail(ptr, i64) noreturn
declare i64 @meadow_prim(i64, i64, ptr, ptr)
declare i64 @meadow_field(i64, i64)
declare i64 @meadow_text(ptr, i64)
declare i64 @meadow_bigint(i64)
declare void @meadow_preempted()
declare void @meadow_gc_poll(i64)
declare void @meadow_candidate(i64)
declare void @meadow_region_shared(i64)
declare void @meadow_region_erased(i64)
declare i64 @meadow_hash(i64, i64)
declare i64 @meadow_global_ready(i64)
declare i64 @meadow_global_get(i64)
declare i64 @meadow_equal(i64, i64, i64, i64)
declare i64 @meadow_string_index_of(i64, i64, i64)
declare i64 @meadow_string_slice(i64, i64, i64)
declare i64 @meadow_get_ref(i64)
declare i64 @meadow_st_set(i64, i64, i64, i64)
declare i64 @meadow_array_push(i64, i64, i64)
declare i64 @meadow_array_concat(i64, i64)
declare i32 @meadow_run(ptr, i32, ptr)
declare i32 @meadow_run_test(ptr, i64, i32, ptr)
declare i64 @meadow_enter(i64, i64)
declare i64 @meadow_detach(i64, i64)
declare i64 @meadow_reattach(i64, i64)
declare i64 @meadow_native(ptr, ptr, i64, i64)
declare i64 @meadow_record(i64, ptr, ptr, ptr)
declare i64 @meadow_select(i64, i64)
declare i64 @meadow_extend(i64, i64, i64, i64)
declare i64 @meadow_update(i64, i64, i64, i64, i64)
declare i64 @meadow_array(i64, ptr, ptr)

";

/// Is `p`, whose left operand is represented as `rep`, done inline rather
/// than by the runtime? Arithmetic on `Int`s and `Float`s, and comparing
/// words the machine can compare directly.
fn inline2(p: Prim, rep: Option<Rep>) -> bool {
    use Prim::*;
    match rep {
        Some(Rep::Int) => matches!(
            p,
            Add | Sub
                | Mul
                | Div
                | Mod
                | BitAnd
                | BitOr
                | BitXor
                | Shl
                | Shr
                | Eq
                | Ne
                | Lt
                | Gt
                | Le
                | Ge
                | IntAdd
                | IntSub
                | IntMul
                | IntDiv
                | IntMod
                | IntEq
                | IntNe
                | IntLt
                | IntLe
                | IntGt
                | IntGe
        ),
        Some(Rep::Float) => matches!(
            p,
            AddF | SubF
                | MulF
                | DivF
                | LtF
                | GtF
                | LeF
                | GeF
                | Eq
                | Ne
                | Lt
                | Gt
                | Le
                | Ge
                | FloatAdd
                | FloatSub
                | FloatMul
                | FloatDiv
                | FloatEq
                | FloatNe
                | FloatLt
                | FloatLe
                | FloatGt
                | FloatGe
        ),
        // Unit, Bool, Char, and the interned names: equal as words, and a
        // `Char` ordered as its code.
        Some(Rep::Bits(d)) if d == desc::UNIT || d == desc::BOOL || d == desc::CHAR => {
            matches!(p, Eq | Ne | Lt | Gt | Le | Ge)
        }
        Some(Rep::Str) => matches!(p, Eq | Ne),
        _ => false,
    }
}

/// The typed forms specialization leaves -- `IntAdd`, `FloatLt` -- as the
/// primitive they are on words already known to be of their type.
fn untyped(p: Prim) -> Prim {
    use Prim::*;
    match p {
        IntAdd => Add,
        IntSub => Sub,
        IntMul => Mul,
        IntDiv => Div,
        IntMod => Mod,
        IntEq | FloatEq => Eq,
        IntNe | FloatNe => Ne,
        IntLt | FloatLt => Lt,
        IntLe | FloatLe => Le,
        IntGt | FloatGt => Gt,
        IntGe | FloatGe => Ge,
        FloatAdd => AddF,
        FloatSub => SubF,
        FloatMul => MulF,
        FloatDiv => DivF,
        p => p,
    }
}

fn lit_desc(l: &Lit) -> i64 {
    match l {
        Lit::Int(_) | Lit::AnyInt(..) => desc::INT,
        Lit::Float(_) | Lit::AnyFloat(..) => desc::FLOAT,
        Lit::Float32(_) => desc::FLOAT32,
        Lit::Word(w, _) => desc::word(*w),
        Lit::Char(_) => desc::CHAR,
        Lit::Bool(_) => desc::BOOL,
        Lit::Unit => desc::UNIT,
        Lit::Sym(_) => desc::STR,
        Lit::Str(_) | Lit::BigInt(_) => desc::REF,
    }
}

/// What every function of a program that can spawn calls the running
/// thread's context, its first parameter: the `Ctx` of `silo/src/ctx.rs`,
/// whose first words are the spill area's address and the shadow chain's
/// head. A thread's context moves between OS threads with it but never
/// within memory, so this holds across any wait -- where a thread-local
/// would be the OS thread's -- and finding it costs nothing: it is already in
/// a register.
const CTX: &str = "%ctx";

/// The calling convention a module's functions are in.
///
/// **`ghccc`** on x86-64: no callee-saved registers, everything in registers,
/// and every `tail call` a jump -- on Windows too, where `tailcc` is not up to
/// an AxCut environment's size.
///
/// **`tailcc`** on aarch64, where `ghccc` cannot be used at all: LLVM gives a
/// GHC-convention function no prologue there -- GHC keeps its own stack -- so
/// the link register is never saved, and it does not make GHC-convention
/// tail calls. A `tail call` became `bl` then `ret`, and the `ret` returned to
/// itself for ever. `tailcc` has ordinary prologues, and LLVM guarantees its
/// marked tail calls on aarch64.
///
/// Either way **nothing is passed on the stack**: a function takes as many
/// words as the convention has registers for ([`CallConv::regs`]), and the
/// rest go through the spill area ([`spill`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallConv {
    Ghc,
    Tail,
}

impl CallConv {
    /// The convention for code that runs on `arch`, as Rust names it:
    /// `x86_64`, `aarch64`.
    pub fn for_arch(arch: &str) -> CallConv {
        match arch {
            "aarch64" => CallConv::Tail,
            _ => CallConv::Ghc,
        }
    }

    /// The convention for code that runs on this machine.
    pub fn host() -> CallConv {
        CallConv::for_arch(std::env::consts::ARCH)
    }

    /// How many words a call passes in registers, and so how many a function
    /// is ever handed as arguments: ten in `ghccc` on x86-64, eight in
    /// `tailcc` on aarch64.
    ///
    /// `tailcc` would pass a ninth and a tenth on the stack, and LLVM's
    /// aarch64 back end gets that wrong twice over (clang 22). The callee
    /// owns what it is passed there and pops it, sixteen bytes at a time; a
    /// caller passing one word is laid out as if the other eight bytes were
    /// still its own, and keeps a local in them, which the callee's tail
    /// call to a function of two words overwrites -- so a value live across
    /// a call of nine words came back as an address. And at `-O0` the caller
    /// does not take the stack pointer back after the callee has popped, so
    /// everything it reads afterwards is sixteen bytes out.
    /// `compiler/meadow-llvm/tests/native.rs` holds it to this.
    pub fn regs(self) -> usize {
        match self {
            CallConv::Ghc => 10,
            CallConv::Tail => 8,
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            CallConv::Ghc => "ghccc",
            CallConv::Tail => "tailcc",
        }
    }
}

/// What does not fit in registers ([`CallConv::regs`]) goes through the running thread's spill area
/// (`meadow_spill_area`): stored just before the call, and loaded by the
/// callee first thing, before anything else could use it.
///
/// The area is the running thread's, and in a program that can spawn its
/// address is word 0 of the context the function was handed -- never an OS
/// thread's, which a thread that waits and carries on elsewhere would leave
/// behind. One that cannot has a single area, a static.
fn spill(f: &mut Fun, ops: &[String], regs: usize, threaded: bool) {
    if ops.len() <= regs {
        return;
    }
    f.spill = f.spill.max(ops.len() - regs);
    let area = f.t();
    f.i(spill_area(&area, threaded));
    for (i, o) in ops.iter().enumerate().skip(regs) {
        let q = f.t();
        f.i(format!(
            "{q} = getelementptr i64, ptr {area}, i64 {}",
            i - regs
        ));
        f.i(format!("store i64 {o}, ptr {q}"));
    }
}

/// `area` set to the spill area's address.
fn spill_area(area: &str, threaded: bool) -> String {
    if threaded {
        format!("{area}.ctx = inttoptr i64 {CTX} to ptr\n  {area} = load ptr, ptr {area}.ctx")
    } else {
        format!("{area} = call ptr @meadow_spill_area()")
    }
}

/// The loads of a function's parameters past the first `regs`, at its entry.
fn spilled_params(params: &[String], regs: usize, threaded: bool) -> String {
    let mut out = String::new();
    if params.len() <= regs {
        return out;
    }
    let _ = writeln!(out, "  {}", spill_area("%spill.at", threaded));
    for (i, p) in params.iter().enumerate().skip(regs) {
        let _ = writeln!(
            out,
            "  {p}.at = getelementptr i64, ptr %spill.at, i64 {}\n  {p} = load i64, ptr {p}.at",
            i - regs
        );
    }
    out
}

/// `[n x ptr]` of `elems`, each written `ptr …` -- `zeroinitializer` when
/// there are none, which LLVM wants for an empty array.
fn array(elems: &[String]) -> String {
    if elems.is_empty() {
        "[0 x ptr] zeroinitializer".into()
    } else {
        format!("[{} x ptr] [{}]", elems.len(), elems.join(", "))
    }
}

fn env_with(env: &HashMap<Name, V>, names: &[Name], vals: &[String]) -> HashMap<Name, V> {
    let mut e = env.clone();
    for (n, v) in names.iter().zip(vals) {
        e.insert(*n, V::Val(v.clone()));
    }
    e
}

/// A function being written.
struct Fun {
    name: String,
    params: Vec<String>,
    body: String,
    allocas: String,
    tmp: usize,
    blk: usize,
    cur: String,
    /// Where each reuse token made in this function came from, by its
    /// operand: see [`Module::build_in`].
    origins: HashMap<String, Origin>,
    /// The most words a call here passes through the spill area.
    spill: usize,
}

/// The block a reuse token holds, as the `switch` that took it apart left
/// it: which constructor, and what each field held and how it is described.
/// Only the reference count and the fields' ownership changed hands; the
/// words themselves are as they were.
#[derive(Clone)]
struct Origin {
    meta: u64,
    fields: Vec<String>,
    descs: Vec<D>,
}

impl Fun {
    fn new(name: String, params: Vec<String>) -> Fun {
        Fun {
            name,
            params,
            body: String::new(),
            allocas: String::new(),
            tmp: 0,
            blk: 0,
            cur: "entry".into(),
            origins: HashMap::new(),
            spill: 0,
        }
    }

    fn t(&mut self) -> String {
        self.tmp += 1;
        format!("%t{}", self.tmp)
    }

    fn b(&mut self) -> String {
        self.blk += 1;
        format!("b{}", self.blk)
    }

    fn i(&mut self, s: String) {
        self.body.push_str("  ");
        self.body.push_str(&s);
        self.body.push('\n');
    }

    fn alloca(&mut self, s: String) {
        self.allocas.push_str("  ");
        self.allocas.push_str(&s);
        self.allocas.push('\n');
    }

    fn label(&mut self, l: &str) {
        let _ = writeln!(self.body, "{l}:");
        self.cur = l.to_string();
    }

    /// `v` as a `ptr`.
    fn ptr(&mut self, v: &str) -> String {
        let p = self.t();
        self.i(format!("{p} = inttoptr i64 {v} to ptr"));
        p
    }
}

/// What a module's `main` runs: the program's entry point, or one of a table
/// of tests.
enum Entries {
    Main(meadow_axcut::Label),
    Tests(Vec<meadow_axcut::Label>),
    /// A library's: a C function for each export, and no `main`.
    Library(Vec<meadow_axcut::Export>),
}

/// The C type a word of representation `rep` crosses the boundary as, and
/// how it becomes the word: `None` for one that cannot cross, a reference.
///
/// | rep | C | to the word |
/// |---|---|---|
/// | `Int` | `int64_t` (or a pointer) | itself |
/// | `Float` | `double` | its bits |
/// | `Float32` | `float` | its bits, in the low half |
/// | a sized integer | its own | sign- or zero-extended |
/// | `Bool` | `uint8_t` | zero-extended |
/// | `Char` | `uint32_t` | zero-extended |
/// | `()` | `void` | `0` |
fn c_type(rep: meadow_axcut::Rep) -> Option<(&'static str, &'static str)> {
    use meadow_axcut::Rep;
    use meadow_rt::num::Width;
    Some(match rep {
        Rep::Int => ("i64", "same"),
        Rep::Float => ("double", "double"),
        Rep::Bits(d) if d == desc::FLOAT32 => ("float", "float"),
        Rep::Bits(d) if d == desc::BOOL => ("i8", "zext"),
        Rep::Bits(d) if d == desc::CHAR => ("i32", "zext"),
        Rep::Bits(d) if d == desc::UNIT => ("void", "void"),
        Rep::Bits(d) if d == desc::word(Width::I8) => ("i8", "sext"),
        Rep::Bits(d) if d == desc::word(Width::I16) => ("i16", "sext"),
        Rep::Bits(d) if d == desc::word(Width::I32) => ("i32", "sext"),
        Rep::Bits(d) if d == desc::word(Width::U8) => ("i8", "zext"),
        Rep::Bits(d) if d == desc::word(Width::U16) => ("i16", "zext"),
        Rep::Bits(d) if d == desc::word(Width::U32) => ("i32", "zext"),
        Rep::Bits(d) if d == desc::word(Width::U64) => ("i64", "same"),
        _ => return None,
    })
}

/// A C function `name` that runs export block `label` of `params`
/// answering `result`: the runtime made ready for this thread, each argument
/// made the word it is, the block entered as a program's entry is -- its
/// continuation the native return -- and the word made what C is answered.
fn export_fn(
    out: &mut String,
    name: &str,
    callee: &str,
    cc: &str,
    ctx: (&str, &str),
    params: &[meadow_axcut::Rep],
    result: meadow_axcut::Rep,
) -> Result<(), String> {
    let mut sig = Vec::new();
    let mut body = String::new();
    let mut words = Vec::new();
    for (i, p) in params.iter().enumerate() {
        let Some((ty, how)) = c_type(*p) else {
            return Err(format!(
                "`{name}`'s argument {i} is not of a type C can pass"
            ));
        };
        let a = format!("%c{i}");
        let w = format!("%w{i}");
        match how {
            "void" => {
                let _ = writeln!(body, "  {w} = add i64 0, 0");
            }
            "same" => {
                sig.push(format!("i64 {a}"));
                let _ = writeln!(body, "  {w} = add i64 {a}, 0");
            }
            "double" => {
                sig.push(format!("double {a}"));
                let _ = writeln!(body, "  {w} = bitcast double {a} to i64");
            }
            "float" => {
                sig.push(format!("float {a}"));
                let _ = writeln!(body, "  {w}b = bitcast float {a} to i32");
                let _ = writeln!(body, "  {w} = zext i32 {w}b to i64");
            }
            ext => {
                sig.push(format!("{ty} {a}"));
                let _ = writeln!(body, "  {w} = {ext} {ty} {a} to i64");
            }
        }
        words.push(w);
    }
    let Some((rty, rhow)) = c_type(result) else {
        return Err(format!("what `{name}` answers is not of a type C can take"));
    };
    let (get, arg) = ctx;
    let n = words.len();
    // The call itself, of its arguments as words in `env`, its answer put
    // after them: what the runtime runs on a segment of its own, so that a
    // failure in it ends the call and not the program C is.
    let _ = write!(
        out,
        "define internal void @\"{name}$run\"(ptr %env) {{\n{get}"
    );
    for i in 0..n {
        let _ = write!(
            out,
            "  %p{i} = getelementptr i64, ptr %env, i64 {i}\n  %v{i} = load i64, ptr %p{i}\n"
        );
    }
    let _ = write!(
        out,
        "  %r = call {cc} i64 {callee}({arg}{}i64 0)\n  %rp = getelementptr i64, ptr %env, i64 {n}\n  store i64 %r, ptr %rp\n  ret void\n}}\n\n",
        (0..n).map(|i| format!("i64 %v{i}, ")).collect::<String>()
    );
    // The C function: its arguments as words, the call run, and the answer
    // -- zero where the call failed -- as C's type.
    let _ = write!(
        out,
        "define dllexport {rty} @\"{name}\"({}) {{\n  call void @meadow_export_enter()\n  %env = alloca [{} x i64]\n{body}",
        sig.join(", "),
        n + 1
    );
    for (i, w) in words.iter().enumerate() {
        let _ = write!(
            out,
            "  %s{i} = getelementptr i64, ptr %env, i64 {i}\n  store i64 {w}, ptr %s{i}\n"
        );
    }
    let _ = write!(
        out,
        "  %rp = getelementptr i64, ptr %env, i64 {n}\n  store i64 0, ptr %rp\n  %failed = call i32 @meadow_export_run(ptr @\"{name}$run\", ptr %env)\n  %r = load i64, ptr %rp\n"
    );
    match rhow {
        "void" => out.push_str("  ret void\n"),
        "same" => out.push_str("  ret i64 %r\n"),
        "double" => out.push_str("  %d = bitcast i64 %r to double\n  ret double %d\n"),
        "float" => out.push_str(
            "  %t = trunc i64 %r to i32\n  %f = bitcast i32 %t to float\n  ret float %f\n",
        ),
        _ => {
            let _ = write!(out, "  %t = trunc i64 %r to {rty}\n  ret {rty} %t\n");
        }
    }
    out.push_str("}\n\n");
    Ok(())
}

/// The helpers as a program gets them: [`helpers_text`], for its layout. One
/// that can tie a knot hands every block whose count went down without
/// reaching zero to the collector, which is where a cycle is noticed
/// (`silo/src/cycles.rs`); one that cannot has no such call anywhere in it.
///
/// A traced program counts nothing, so it has no sharing or erasing to do,
/// and its `@mw.field` shares nothing: under Silo's layout the count is the
/// mark a collection leaves, which a share would move.
fn helpers(l: &Layout, tracing: bool, cycles: bool, regions: bool) -> String {
    if tracing {
        return field_text(l, false);
    }
    let c = count_ty(l);
    // The test before the call is inline, and on a program that makes no
    // cycles it is the whole cost, so it is one mask and one compare against
    // a constant. A block is worth keeping only if it is a `Ref` or a mutable
    // array -- the two kinds a cycle must contain, marked as such when they
    // are built -- and only the first time its count goes down, since it is
    // coloured until a collection looks at it. Both questions are in the same
    // word, so both are asked at once: `mutable` set and the colour black,
    // which is zero. `silo/src/cycles.rs` says why those two kinds, and the
    // layout has the bits.
    let candidate = if cycles {
        format!(
            "  %w1a = getelementptr i64, ptr %p, i64 {word}
  %w1 = load i64, ptr %w1a
  %ask = and i64 %w1, {ask}
  %want = icmp eq i64 %ask, {want}
  br i1 %want, label %cand, label %done, !prof !0
cand:
  call void @meadow_candidate(i64 %v)
  br label %done
",
            word = l.mutable.word,
            ask = l.mutable.bits_in_place() | l.colour.bits_in_place(),
            want = l.mutable.bits_in_place(),
        )
    } else {
        "  br label %done\n".to_string()
    };
    // A block inside a compact region carries far more references than it
    // has, so the count a share or an erase has already loaded says whether
    // it is touching one -- and the region has to be told, since that is what
    // keeps it alive while anything points into it. `silo/src/region.rs` says
    // why, and the layout has the number.
    let floor = l.region_floor;
    let share_tail = if regions {
        format!(
            "  %inreg = icmp ugt {c} %rc2, {floor}
  br i1 %inreg, label %rshare, label %done, !prof !0
rshare:
  call void @meadow_region_shared(i64 %v)
  br label %done
"
        )
    } else {
        "  br label %done\n".to_string()
    };
    // The erase side asks before it takes one away, and a block in a region
    // is never a candidate for the cycle collector: a region is closed, and
    // nothing in it can change.
    let erase_tail = if regions {
        format!(
            "  %inreg = icmp ugt {c} %rc, {floor}
  br i1 %inreg, label %rerase, label %noreg, !prof !0
rerase:
  call void @meadow_region_erased(i64 %v)
  br label %done
noreg:
{candidate}"
        )
    } else {
        candidate
    };
    // Counting happens everywhere and both of these are rare, so say which
    // way the tests go: the calls and their setup are laid out away from the
    // path every share and every decrement takes.
    let cold = if cycles || regions {
        "\n!0 = !{!\"branch_weights\", i32 1, i32 4096}\n"
    } else {
        ""
    };
    helpers_text(l)
        .replace("; region-share\n", &share_tail)
        .replace("; candidate\n", &erase_tail)
        + cold
}

/// Sharing and erasing, the paper's `share` and `erase`, as the module's own
/// small functions: skip the null word and a small value (no block), and a
/// value whose descriptor says it is no reference; add to the count, or take
/// from it -- handing the block to the runtime when it was the last. And
/// `@mw.field` ([`field_text`]).
fn helpers_text(l: &Layout) -> String {
    let c = count_ty(l);
    let (tag, null, r) = (l.imm_mask, l.null, desc::REF);
    format!(
        "\
define internal void @mw.share(i64 %v, {c} %n) alwaysinline {{
entry:
  %low = and i64 %v, {tag}
  %even = icmp eq i64 %low, 0
  %nz = icmp ne i64 %v, {null}
  %block = and i1 %even, %nz
  br i1 %block, label %go, label %done
go:
  %p = inttoptr i64 %v to ptr
  %rc = load {c}, ptr %p
  %rc2 = add {c} %rc, %n
  store {c} %rc2, ptr %p
; region-share
done:
  ret void
}}

define internal void @mw.erase(i64 %ctx, i64 %v) alwaysinline {{
entry:
  %low = and i64 %v, {tag}
  %even = icmp eq i64 %low, 0
  %nz = icmp ne i64 %v, {null}
  %block = and i1 %even, %nz
  br i1 %block, label %go, label %done
go:
  %p = inttoptr i64 %v to ptr
  %rc = load {c}, ptr %p
  %last = icmp eq {c} %rc, 0
  br i1 %last, label %free, label %dec
free:
  call void @meadow_free(i64 %ctx, i64 %v)
  br label %done
dec:
  %rc2 = sub {c} %rc, 1
  store {c} %rc2, ptr %p
; candidate
done:
  ret void
}}

define internal void @mw.share_d(i64 %v, i64 %d, {c} %n) alwaysinline {{
entry:
  %isref = icmp eq i64 %d, {r}
  br i1 %isref, label %go, label %done
go:
  call void @mw.share(i64 %v, {c} %n)
  br label %done
done:
  ret void
}}

define internal void @mw.erase_d(i64 %ctx, i64 %v, i64 %d) alwaysinline {{
entry:
  %isref = icmp eq i64 %d, {r}
  br i1 %isref, label %go, label %done
go:
  call void @mw.erase(i64 %ctx, i64 %v)
  br label %done
done:
  ret void
}}

{field}",
        field = field_text(l, true),
    )
}

/// `@mw.field`, a field read from a block whose shape is not known here:
/// shared, when `shares`. Where its descriptor is depends on the layout:
/// sixteen to a word after the header, or the first few in a header word.
fn field_text(l: &Layout, shares: bool) -> String {
    let per = l.descs_per_word;
    let c = count_ty(l);
    // Where field %i's descriptor is, and where a mixed block's fields
    // begin: as `Layout::desc_at` and `Layout::first_field` say.
    let mixed = if l.inline_descs == 0 {
        format!(
            "  %w0 = load i64, ptr %p
  %len = lshr i64 %w0, {len_shift}
  %len15 = add i64 %len, {per_less}
  %dws = lshr i64 %len15, {per_log}
  %mfirst = add i64 %dws, {header}
  %slot0 = lshr i64 %i, {per_log}
  %slot = add i64 %slot0, {header}
  %dwp = getelementptr i64, ptr %p, i64 %slot
  %dw = load i64, ptr %dwp
  %nib = and i64 %i, {per_less}
  %shift = shl i64 %nib, {bits_log}
",
            len_shift = l.len.shift,
            per_less = per - 1,
            per_log = per.trailing_zeros(),
            header = l.header_words,
            bits_log = l.desc_bits.trailing_zeros(),
        )
    } else {
        format!(
            "  %w0p = getelementptr i64, ptr %p, i64 {len_word}
  %w0 = load i64, ptr %w0p
  %len0 = lshr i64 %w0, {len_shift}
  %len = and i64 %len0, {len_mask}
  %over = icmp ugt i64 %len, {inline}
  %past0 = sub i64 %len, {inline}
  %past = select i1 %over, i64 %past0, i64 0
  %len15 = add i64 %past, {per_less}
  %dws = lshr i64 %len15, {per_log}
  %mfirst = add i64 %dws, {header}
  %out = icmp uge i64 %i, {inline}
  %j = sub i64 %i, {inline}
  %oslot0 = lshr i64 %j, {per_log}
  %oslot = add i64 %oslot0, {header}
  %slot = select i1 %out, i64 %oslot, i64 {inline_word}
  %dwp = getelementptr i64, ptr %p, i64 %slot
  %dw = load i64, ptr %dwp
  %onib = and i64 %j, {per_less}
  %oshift = shl i64 %onib, {bits_log}
  %ishift0 = shl i64 %i, {bits_log}
  %ishift = add i64 %ishift0, {inline_shift}
  %shift = select i1 %out, i64 %oshift, i64 %ishift
",
            len_word = l.len.word,
            len_shift = l.len.shift,
            len_mask = l.len.mask(),
            inline = l.inline_descs,
            inline_word = l.inline_word,
            inline_shift = l.inline_shift,
            per_less = per - 1,
            per_log = per.trailing_zeros(),
            header = l.header_words,
            bits_log = l.desc_bits.trailing_zeros(),
        )
    };
    let share = if shares {
        format!(
            "  call void @mw.share_d(i64 %x, i64 %d, {c} 1)
"
        )
    } else {
        String::new()
    };
    format!(
        "\
; Field %i of the block at %v, shared: `meadow_field` without the call. A
; uniform block's fields start at word 2 and share word 1's descriptor; any
; other's start past its descriptor words, sixteen four-bit descriptors to a
; word. Called with a constant %i, all but the loads folds away.
define internal i64 @mw.field(i64 %v, i64 %i) alwaysinline {{
entry:
  %p = inttoptr i64 %v to ptr
  %w1p = getelementptr i64, ptr %p, i64 {w1}
  %w1 = load i64, ptr %w1p
  %uni = and i64 %w1, {uniform}
  %isuni = icmp ne i64 %uni, 0
  br i1 %isuni, label %uniform, label %mixed
uniform:
  %ud0 = lshr i64 %w1, {ud_shift}
  %ud = and i64 %ud0, {ud_mask}
  br label %load
mixed:
{mixed}  %md0 = lshr i64 %dw, %shift
  %md = and i64 %md0, {desc_mask}
  br label %load
load:
  %first = phi i64 [ {uniform_first}, %uniform ], [ %mfirst, %mixed ]
  %d = phi i64 [ %ud, %uniform ], [ %md, %mixed ]
  %at = add i64 %first, %i
  %xp = getelementptr i64, ptr %p, i64 %at
  %x = load i64, ptr %xp
{share}  ret i64 %x
}}

",
        w1 = l.uniform.word,
        uniform = l.uniform.bits_in_place(),
        ud_shift = l.elem_desc.shift,
        ud_mask = l.elem_desc.mask(),
        desc_mask = l.desc_mask(),
        uniform_first = l.first_field(0, true),
    )
}

/// [`invoke1`] in the module's convention, handing the method it calls the
/// running thread's context where the program has one to hand.
fn invokers(l: &Layout, cc: &str, threaded: bool) -> String {
    let text = invoke1(l).replace("ghccc", cc);
    if threaded {
        text.replace("  %slot = ", "  %ctx = call i64 @meadow_ctx()\n  %slot = ")
            .replace(" i64 %fp(i64 %obj", " i64 %fp(i64 %ctx, i64 %obj")
    } else {
        text
    }
}

/// How the runtime runs a closure of one argument -- the code after a stack
/// segment primitive (`silo/src/segments.rs`) -- when the methods are in a
/// calling convention Rust cannot call: method 0 of `obj`, found as an
/// `invoke` finds it, called from the C convention. Written for `ghccc`, and
/// given the module's own convention where it is used.
fn invoke1(l: &Layout) -> String {
    // The first method's index, as [`Module::meta_of`] finds it.
    let meta = format!(
        "  %low = and i64 %obj, {tag}
  %odd = icmp ne i64 %low, 0
  br i1 %odd, label %imm, label %blk
imm:
  %m1 = lshr i64 %obj, {imm_shift}
  br label %go
blk:
  %p = inttoptr i64 %obj to ptr
  %q = getelementptr i64, ptr %p, i64 {word}
  %w = load i64, ptr %q
{read}  br label %go
go:
  %m = phi i64 [ %m1, %imm ], [ %m2, %blk ]
",
        tag = l.imm_mask,
        imm_shift = l.imm_shift,
        word = l.meta.word,
        read = if l.meta.tops() {
            format!("  %m2 = lshr i64 %w, {}\n", l.meta.shift)
        } else {
            format!(
                "  %m2s = lshr i64 %w, {}\n  %m2 = and i64 %m2s, {}\n",
                l.meta.shift,
                l.meta.mask()
            )
        },
    );
    format!(
        "\
define i64 @meadow_invoke1(i64 %obj, i64 %arg) {{
entry:
{meta}  %slot = getelementptr ptr, ptr @meadow_methods, i64 %m
  %fp = load ptr, ptr %slot
  %r = call ghccc i64 %fp(i64 %obj, i64 %arg)
  ret i64 %r
}}

define i64 @meadow_apply(i64 %obj, i64 %arg, i64 %ev) {{
entry:
{meta}  %slot = getelementptr ptr, ptr @meadow_methods, i64 %m
  %fp = load ptr, ptr %slot
  %r = call ghccc i64 %fp(i64 %obj, i64 %arg, i64 0, i64 %ev)
  ret i64 %r
}}

"
    )
}

/// The primitives with an entry of their own in the runtime, and which of
/// their arguments it wants a descriptor beside. These are the ones a program
/// does so often that building an argument array for [`Module::generic`] costs
/// more than the primitive does: `wordfreq` alone makes three and a half
/// million of these calls.
fn direct(p: Prim) -> Option<(&'static str, Descs)> {
    Some(match p {
        Prim::Hash => ("meadow_hash", Descs::Each),
        Prim::Eq => ("meadow_equal", Descs::Each),
        Prim::StringIndexOf => ("meadow_string_index_of", Descs::None),
        Prim::StringSlice => ("meadow_string_slice", Descs::None),
        // The element's descriptor, and no other's: the array and the index
        // are what they are.
        Prim::StSetArray => ("meadow_st_set", Descs::Last),
        // These two consume the array they grow, which is what lets them grow
        // it in place: see [`consumes`].
        // A top-level value is asked whether it is there, and read, at
        // every mention of it: a program that passes dictionaries does
        // little else.
        Prim::GlobalReady => ("meadow_global_ready", Descs::None),
        Prim::GlobalGet => ("meadow_global_get", Descs::None),
        Prim::ArrayPush => ("meadow_array_push", Descs::Last),
        Prim::ArrayConcat => ("meadow_array_concat", Descs::None),
        _ => return None,
    })
}

/// The argument a primitive consumes rather than borrows: the memory pass's
/// rule, `meadow_axcut::linear::consumes`.
pub use crate::linear::consumes;

/// Which arguments of a [`direct`] primitive carry a descriptor.
#[derive(Clone, Copy, PartialEq)]
enum Descs {
    None,
    Each,
    Last,
}
