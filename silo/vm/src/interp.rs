//! **The interpreter**: the linear program (`meadow_axcut::linear`) run
//! statement by statement, doing what `meadow_llvm::emit` writes code to do.
//!
//! It follows the compiler closely, because both run on the same runtime and
//! must agree with it on every word: a block is laid out as the compiler lays
//! it out, a closure's methods are numbered in one table and found by the
//! number in its header, a reference is shared and erased where the linear
//! program says, and a primitive goes to the runtime through the entry the
//! compiled code would call -- `meadow_prim`, or one of the entries of their
//! own the commonest have.
//!
//! **Loading** does once what the compiler does once: each block of the
//! linear program becomes a [`Proc`], its names numbered as slots of a frame
//! of words (its parameters first), each use of a name carrying how the name
//! is described -- a constant, or the slot of the type variable's descriptor
//! that says at run time -- and each primitive already sorted into how it is
//! done. Running is then indexing, with nothing looked up by name.
//!
//! **Calls.** A jump or an invoke is a tail call, done by carrying on in this
//! function's loop with the callee's code and the frame of words reused. A
//! *frame* -- the continuation a function makes for its own non-tail call --
//! is not built: it stays a value of this interpreter's ([`V::Frame`]), the
//! call is a call of [`Vm::call`] with the null continuation, which returns
//! natively, and the frame's code runs after it with what came back. That is
//! the compiler's "frames are the native stack", and for the same reason: a
//! frame may borrow what its block borrowed, which as an object it could not.
//! A frame used any other way -- captured, passed as anything but the
//! continuation of a call -- is built as the closure it would have been.
//!
//! **Memory.** Counted, every share, erase, release and reuse the linear
//! program writes is done as the compiled code does it. Traced, none is: a
//! function's entry is a safe point, where a collection that is due runs with
//! the function's parameters on the shadow chain, and every call waiting for
//! an answer has put what its frame holds there too -- see
//! `meadow_silo::trace`.

use meadow_axcut::linear::{self, L, LBlock};
use meadow_axcut::{Extern, Label, NO_DESC, Name, Program, Rep, VarId};
use meadow_intern::InternedString;
use meadow_rt::{Lit, Prim, desc};
use meadow_silo::heap::{self, Word};
use smallvec::SmallVec;

mod code;
pub mod debug;
use code::{Code, Ins, WideI};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A name's place in its procedure's frame of words.
type Slot = u32;

/// How a value is described to the runtime: known when the program is
/// loaded, or the value of a type variable's descriptor, in a slot.
#[derive(Clone, Copy, Debug)]
enum D {
    K(i64),
    S(Slot),
}

/// A use of a name: where it is, and how it is described.
#[derive(Clone, Copy, Debug)]
struct U {
    s: Slot,
    d: D,
}

/// What a slot holds: a word, a frame not built, or nothing yet.
#[derive(Default)]
enum V {
    #[default]
    Empty,
    Val(Word),
    Frame(Box<Frame>),
}

/// How a frame holds one of what it captured.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hold {
    Owned,
    Borrowed,
    Token,
}

/// A continuation made for a non-tail call, kept as what it captured.
struct Frame {
    /// Its code, which takes the captures and then the call's answer.
    proc: u32,
    /// Its place in the method table, for building it after all.
    base: u32,
    captured: SmallVec<[(V, i64, Hold); 4]>,
}

type Args = SmallVec<[V; 6]>;
/// A call's slots, kept in the native frame of the call that runs it: what
/// most procedures need fits, and nothing is allocated for a call.
type Env = SmallVec<[V; 16]>;
type Words = SmallVec<[Word; 8]>;
type Descs = SmallVec<[i64; 8]>;

/// A block of the linear program, ready to run.
struct Proc {
    slots: usize,
    /// How each parameter is described, in this procedure's own slots.
    params: Vec<D>,
    /// Where its code begins.
    entry: u32,
    /// What it is, for the disassembler and the debugger: a definition's
    /// name, or the method, frame or packed code of one.
    name: String,
    /// The variable each register holds, by its number -- for a debugger's
    /// locals -- and how it is described.
    slot_vars: Vec<Option<u32>>,
    slot_descs: Vec<D>,
    /// The register holding the continuation it returns to, if it has one:
    /// what a debugger tells a frame by, when frames are on the heap.
    ret_slot: Option<Slot>,
}

/// A literal operand.
#[derive(Clone, Copy)]
enum K {
    W(Word),
    Text(&'static [u8]),
    Big(i64),
}

/// How a primitive is done.
#[derive(Clone, Copy)]
enum PK {
    /// On two words, inline: the primitive without its type, and whether
    /// they are floats.
    Inline(Prim, bool),
    Neg,
    /// Through an entry of its own in the runtime.
    Direct,
    /// `ThreadSpawn`, told how what the thread answers is described.
    Spawn(D),
    /// Through `meadow_prim`.
    Generic,
}

enum Op {
    Lit(K),
    Prim(Prim, PK),
    PrimK(Prim, PK, K, i64),
    Field(i64),
    Array,
    /// A record: its fields in the labels' order, and the labels.
    Record(Vec<usize>, Vec<i64>),
    Select(i64),
    SelectAt(i64),
    Update(i64, i64),
    Extend(i64),
    Native(&'static CStr, &'static CStr),
}

/// A binary primitive done inline, its operands' type part of it.
#[derive(Clone, Copy, Debug)]
enum B {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    FAdd,
    FSub,
    FMul,
    FDiv,
    FEq,
    FNe,
    FLt,
    FGt,
    FLe,
    FGe,
}

impl B {
    /// The primitive `p` as [`inline2`] allows it, on floats or not.
    fn of(p: Prim, float: bool) -> Option<B> {
        use Prim as P;
        Some(match (untyped(p), float) {
            (P::Add, _) => B::Add,
            (P::Sub, _) => B::Sub,
            (P::Mul, _) => B::Mul,
            (P::Div, _) => B::Div,
            (P::Mod, _) => B::Mod,
            (P::BitAnd, _) => B::And,
            (P::BitOr, _) => B::Or,
            (P::BitXor, _) => B::Xor,
            (P::Shl, _) => B::Shl,
            (P::Shr, _) => B::Shr,
            (P::AddF, _) => B::FAdd,
            (P::SubF, _) => B::FSub,
            (P::MulF, _) => B::FMul,
            (P::DivF, _) => B::FDiv,
            (P::LtF, _) | (P::Lt, true) => B::FLt,
            (P::GtF, _) | (P::Gt, true) => B::FGt,
            (P::LeF, _) | (P::Le, true) => B::FLe,
            (P::GeF, _) | (P::Ge, true) => B::FGe,
            (P::Eq, true) => B::FEq,
            (P::Ne, true) => B::FNe,
            (P::Eq, false) => B::Eq,
            (P::Ne, false) => B::Ne,
            (P::Lt, false) => B::Lt,
            (P::Gt, false) => B::Gt,
            (P::Le, false) => B::Le,
            (P::Ge, false) => B::Ge,
            _ => return None,
        })
    }

    #[inline(always)]
    fn run(self, a: Word, b: Word) -> Word {
        let (x, y) = (a as i64, b as i64);
        let (fx, fy) = (f64::from_bits(a), f64::from_bits(b));
        match self {
            B::Add => x.wrapping_add(y) as Word,
            B::Sub => x.wrapping_sub(y) as Word,
            B::Mul => x.wrapping_mul(y) as Word,
            B::Div | B::Mod => {
                let div = matches!(self, B::Div);
                if y == 0 {
                    fail(if div {
                        "division by zero"
                    } else {
                        "modulo by zero"
                    });
                }
                if y == -1 {
                    return if div { x.wrapping_neg() as Word } else { 0 };
                }
                (if div { x / y } else { x % y }) as Word
            }
            B::And => a & b,
            B::Or => a | b,
            B::Xor => a ^ b,
            B::Shl => (x << (b & 63)) as Word,
            B::Shr => (x >> (b & 63)) as Word,
            B::Eq => Word::from(x == y),
            B::Ne => Word::from(x != y),
            B::Lt => Word::from(x < y),
            B::Gt => Word::from(x > y),
            B::Le => Word::from(x <= y),
            B::Ge => Word::from(x >= y),
            B::FAdd => (fx + fy).to_bits(),
            B::FSub => (fx - fy).to_bits(),
            B::FMul => (fx * fy).to_bits(),
            B::FDiv => (fx / fy).to_bits(),
            B::FEq => Word::from(fx == fy),
            B::FNe => Word::from(fx != fy),
            B::FLt => Word::from(fx < fy),
            B::FGt => Word::from(fx > fy),
            B::FLe => Word::from(fx <= fy),
            B::FGe => Word::from(fx >= fy),
        }
    }
}

enum Test {
    Truth,
    Prim(Prim, PK),
    PrimK(Prim, PK, K, i64),
}

struct Arm {
    tag: u64,
    fields: Vec<U>,
    keep: bool,
    reuse: Option<Slot>,
    body: I,
}

/// A statement of the linear program, its names as slots.
enum I {
    Share(U, Box<I>),
    Erase(U, Box<I>),
    Rename(Vec<(Slot, Slot)>, Box<I>),
    Let {
        dst: Slot,
        tag: u32,
        fields: Vec<U>,
        reuse: Option<Slot>,
        rest: Box<I>,
    },
    Clean(Slot, Box<I>),
    DropReuse {
        src: Slot,
        fields: Vec<D>,
        token: Slot,
        rest: Box<I>,
    },
    Switch {
        scrutinee: Slot,
        arms: Vec<Arm>,
        default: Box<I>,
        keep_default: bool,
    },
    New {
        dst: Slot,
        captures: Vec<U>,
        base: u32,
        /// For a frame left unbuilt: its code, and how it holds each capture.
        frame: Option<(u32, Vec<Hold>)>,
        rest: Box<I>,
    },
    Jump {
        proc: u32,
        args: Vec<Slot>,
    },
    Invoke {
        target: Slot,
        tag: u32,
        args: Vec<Slot>,
    },
    Prim {
        op: Op,
        args: Vec<U>,
        result: Option<Slot>,
        rest: Box<I>,
    },
    Branch {
        test: Test,
        args: Vec<U>,
        no: Box<I>,
        yes: Box<I>,
    },
    /// A binary primitive done inline, on two slots or a slot and a word:
    /// what most of a loop is, given an instruction of its own.
    Bin {
        op: B,
        a: Slot,
        b: Slot,
        dst: Slot,
        rest: Box<I>,
    },
    BinK {
        op: B,
        a: Slot,
        k: Word,
        dst: Slot,
        rest: Box<I>,
    },
    IfBin {
        op: B,
        a: Slot,
        b: Slot,
        no: Box<I>,
        yes: Box<I>,
    },
    IfBinK {
        op: B,
        a: Slot,
        k: Word,
        no: Box<I>,
        yes: Box<I>,
    },
    /// A non-tail call whose continuation is a frame made for it and used
    /// for nothing else: the callee called natively with the null
    /// continuation, and the frame's code, compiled into this procedure's
    /// slots -- its captures where they already are -- run on what comes
    /// back. No frame is made: what native code does.
    Call {
        callee: Callee,
        /// The arguments but the continuation.
        args: Vec<Slot>,
        /// What the frame would have held, for the shadow chain.
        held: Vec<(U, Hold)>,
        answer: Slot,
        then: Box<I>,
    },
    /// `Enter`, `Detach` or `Reattach`, the code after it packed as method
    /// `base` of a closure of `captures`.
    Segment {
        prim: Prim,
        arg: U,
        base: u32,
        captures: Vec<U>,
    },
    Error(&'static str),
    /// Where the rest was written: for the line table.
    Mark(meadow_rt::Loc, Box<I>),
}

#[derive(Clone, Copy)]
enum Callee {
    Proc(u32),
    Method { target: Slot, tag: u32 },
}

/// The program, ready to run.
pub struct Vm {
    program: &'static Program,
    /// Where it begins: its entry, or each of its tests.
    entries: Vec<u32>,
    is_tests: bool,
    procs: Vec<Proc>,
    code: Code,
    /// Every method, by its place in the table a closure's header indexes,
    /// and how many of its parameters are the closure's captures.
    methods: Vec<(u32, usize)>,
    syms: HashMap<InternedString, usize>,
    tracing: bool,
    /// Whether a debugger watches: then every activation keeps a record, and
    /// every instruction is a place it may stop.
    debugging: bool,
    /// Whether a call that begins with too little stack left runs on a new
    /// piece of it: see `meadow_silo::stack`.
    growable: bool,
    /// Whether a frame waiting on a call puts what it holds on the shadow
    /// chain: when continuations can be abandoned, or memory is traced.
    shadow: bool,
}

// Safety: the program is read-only once loaded; a `Vm` is shared by the
// runtime's threads only to be read.
unsafe impl Sync for Vm {}
unsafe impl Send for Vm {}

static VM: OnceLock<Vm> = OnceLock::new();

/// Which of the entries runs.
static CHOSEN: AtomicUsize = AtomicUsize::new(0);

/// Whether the program runs under a debugger: see [`debug`].
static DEBUGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Run the program under a debugger, which [`debug::start`] connects.
pub fn debugging() {
    DEBUGGING.store(true, Ordering::Relaxed);
}

pub fn vm() -> &'static Vm {
    VM.get().expect("the program was loaded")
}

/// Run entry `i` of a program of tests, not the first.
pub fn choose(i: usize) {
    CHOSEN.store(i, Ordering::Relaxed);
}

/// Make `image`'s program linear, compile it to procedures, and tell the
/// runtime what it needs to know of it.
pub fn load(image: meadow_axcut::image::Image) -> Result<(), String> {
    let began = std::time::Instant::now();
    let program: &'static Program = Box::leak(Box::new(image.program));
    let is_tests = !image.tests.is_empty();
    let entries = if is_tests {
        image.tests
    } else {
        vec![program.entry.ok_or("the program has no entry point")?]
    };
    let lin = image.linear;
    // What a test answers is not printed; what the program does is.
    let result = match (is_tests, program.entry) {
        (false, Some(e)) => program
            .results
            .get(&e)
            .and_then(|r| r.desc())
            .unwrap_or(desc::ANY),
        _ => desc::ANY,
    };
    let mut c = Compiler {
        program,
        news: HashMap::new(),
        codes: HashMap::new(),
        labels: HashMap::new(),
        procs: Vec::new(),
        methods: Vec::new(),
        syms: HashMap::new(),
        threads: false,
        detaches: false,
        code: Code::default(),
        naming: String::new(),
    };
    // Each definition's procedure is numbered first, so that a jump to one
    // not yet compiled knows where it will be.
    let blocks: Vec<(Label, String, &'static LBlock)> = lin
        .into_iter()
        .map(|(i, label, b)| {
            let d = &program.defs[i];
            (
                label,
                format!("{}.{}", d.module, d.name),
                &*Box::leak(Box::new(b)),
            )
        })
        .collect();
    for (i, (label, ..)) in blocks.iter().enumerate() {
        c.labels.insert(*label, i as u32);
        c.procs.push(None);
    }
    for (i, (_, name, b)) in blocks.iter().enumerate() {
        let p = c.proc(b, name.clone());
        c.procs[i] = Some(p);
    }
    let entries = entries
        .iter()
        .map(|l| {
            c.labels
                .get(l)
                .copied()
                .ok_or("an entry the program does not have")
        })
        .collect::<Result<Vec<u32>, _>>()?;
    let tracing = image.config.memory == meadow_axcut::config::Memory::Trace;
    let growable = image.config.stack == meadow_axcut::config::Stack::Growable;
    // What loading made of the program, for whoever is making it faster.
    if std::env::var_os("MEADOW_VM_STATS").is_some() {
        eprintln!(
            "vm: loaded in {:?}; {} procedures, {} methods, {} instructions; {:?}",
            began.elapsed(),
            c.procs.len(),
            c.methods.len(),
            c.code.ins.len(),
            c.code.census()
        );
        let slots: Vec<usize> = c.procs.iter().flatten().map(|p| p.slots).collect();
        let over = slots.iter().filter(|n| **n > 16).count();
        eprintln!(
            "vm: {over} of {} procedures have over 16 slots; the most is {}",
            slots.len(),
            slots.iter().max().unwrap_or(&0)
        );
    }
    let threads = c.threads;
    let vm = Vm {
        program,
        entries,
        is_tests,
        procs: c.procs.into_iter().map(|p| p.expect("compiled")).collect(),
        code: c.code,
        methods: c.methods,
        syms: c.syms,
        tracing,
        debugging: DEBUGGING.load(Ordering::Relaxed),
        growable,
        shadow: tracing || c.detaches,
    };
    // Safety: set before the runtime starts, which reads them.
    unsafe {
        crate::meadow_threaded = u8::from(threads);
        crate::meadow_cycles = u8::from(!tracing);
        crate::meadow_gc = u8::from(tracing);
        crate::meadow_result_desc = result;
    }
    meadow_silo::show::install(vm.names());
    VM.set(vm)
        .map_err(|_| "a program was loaded twice".to_string())
}

// --- loading ------------------------------------------------------------------

struct Compiler {
    program: &'static Program,
    /// Where each `new` statement's methods are in the table, by its address:
    /// compiled once however many times the code around it is (a frame's
    /// code is, once in its place and once where its call is).
    news: HashMap<usize, (u32, Vec<u32>)>,
    /// Each stack-segment primitive's packed code, by its body's address: its
    /// place in the table, and what it captures.
    codes: HashMap<usize, (u32, Vec<Name>)>,
    labels: HashMap<Label, u32>,
    procs: Vec<Option<Proc>>,
    methods: Vec<(u32, usize)>,
    syms: HashMap<InternedString, usize>,
    threads: bool,
    detaches: bool,
    /// Every procedure's instructions.
    code: Code,
    /// The name of the procedure being compiled, which what it makes is
    /// named after.
    naming: String,
}

/// One procedure's names, as slots. A name read but never bound has a slot
/// all the same, which is empty when it is read, and says so then.
struct Scope {
    slots: HashMap<Name, Slot>,
    /// The names of frames made here, which may be left unbuilt.
    frames: std::collections::HashSet<Name>,
    /// Frames never made, for the call ahead to take: see `direct_ahead`.
    pending: HashMap<Name, Pending>,
}

/// A frame whose one use is as the continuation of a call a little ahead.
struct Pending {
    method: &'static LBlock,
    captures: Vec<U>,
    holds: Vec<Hold>,
}

impl Scope {
    fn at(&mut self, n: Name) -> Slot {
        let next = self.slots.len() as Slot;
        *self.slots.entry(n).or_insert(next)
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

impl Compiler {
    /// `b` as a procedure, named `name`: its parameters are its first slots,
    /// and its code goes into the array of all of it.
    fn proc(&mut self, b: &'static LBlock, name: String) -> Proc {
        let outer = std::mem::replace(&mut self.naming, name.clone());
        let mut scope = Scope {
            slots: HashMap::new(),
            frames: Default::default(),
            pending: HashMap::new(),
        };
        for p in &b.params {
            scope.at(*p);
        }
        let params = b.params.iter().map(|p| self.desc(*p, &scope)).collect();
        let body = self.stmt(&b.body, &mut scope);
        let entry = self.code.here();
        self.code.emit(body, None);
        self.naming = outer;
        // Which variable is in which register: the lowest numbered of the
        // names that share one, which is the one a person wrote before the
        // copies the compiler made of it.
        let n = scope.slots.len();
        let mut slot_vars: Vec<Option<u32>> = vec![None; n];
        let mut slot_descs = vec![D::K(desc::ANY); n];
        for (name, slot) in &scope.slots {
            let i = *slot as usize;
            if linear::is_token(*name) || slot_vars[i].is_some_and(|v| v <= name.0) {
                continue;
            }
            slot_vars[i] = Some(name.0);
            slot_descs[i] = self.desc(*name, &scope);
        }
        let ret_slot = scope
            .slots
            .iter()
            .filter(|(name, _)| self.program.returns.contains(name))
            .map(|(_, slot)| *slot)
            .min();
        Proc {
            slots: n,
            params,
            entry,
            name,
            slot_vars,
            slot_descs,
            ret_slot,
        }
    }

    fn add_proc(&mut self, p: Proc) -> u32 {
        self.procs.push(Some(p));
        (self.procs.len() - 1) as u32
    }

    fn sym(&mut self, s: InternedString) -> i64 {
        let n = self.syms.len();
        *self.syms.entry(s).or_insert(n) as i64
    }

    /// How `n` is described, as `meadow_llvm::emit` describes it.
    fn desc(&self, n: Name, scope: &Scope) -> D {
        if linear::is_token(n) {
            return D::K(desc::INT);
        }
        D::K(match self.program.reps.get(&n) {
            Some(Rep::Ref) | None => desc::REF,
            Some(Rep::Int) => desc::INT,
            Some(Rep::Float) => desc::FLOAT,
            Some(Rep::Str) => desc::STR,
            Some(Rep::Bits(d)) => *d,
            Some(Rep::Var(d)) if *d == NO_DESC => desc::ANY,
            Some(Rep::Var(d)) => match scope.slots.get(&VarId(*d)) {
                Some(s) => return D::S(*s),
                None => desc::ANY,
            },
            Some(Rep::Unknown) => desc::ANY,
        })
    }

    fn use_(&self, n: Name, scope: &mut Scope) -> U {
        let s = scope.at(n);
        U {
            s,
            d: self.desc(n, scope),
        }
    }

    fn uses(&self, ns: &[Name], scope: &mut Scope) -> Vec<U> {
        ns.iter().map(|n| self.use_(*n, scope)).collect()
    }

    fn rep(&self, n: Name) -> Option<Rep> {
        self.program.reps.get(&n).copied()
    }

    fn stmt(&mut self, l: &'static L, scope: &mut Scope) -> I {
        match l {
            L::Share(n, rest) => {
                let u = self.use_(*n, scope);
                I::Share(u, Box::new(self.stmt(rest, scope)))
            }
            L::Erase(n, rest) => {
                let u = self.use_(*n, scope);
                I::Erase(u, Box::new(self.stmt(rest, scope)))
            }
            L::Rename(binds, rest) => {
                let mut pairs: Vec<(Slot, Slot)> = Vec::new();
                for (to, from) in binds.iter().filter(|(to, from)| to != from) {
                    // A pending frame has no slot: its new name is pending.
                    if let Some(pd) = scope.pending.remove(from) {
                        scope.pending.insert(*to, pd);
                        continue;
                    }
                    if scope.frames.contains(from) {
                        scope.frames.insert(*to);
                    }
                    let f = scope.at(*from);
                    pairs.push((scope.at(*to), f));
                }
                if pairs.is_empty() {
                    return self.stmt(rest, scope);
                }
                I::Rename(pairs, Box::new(self.stmt(rest, scope)))
            }
            L::Let {
                name,
                tag,
                fields,
                reuse,
                rest,
            } => {
                let fields = self.uses(fields, scope);
                let reuse = reuse.map(|t| scope.at(t));
                let dst = scope.at(*name);
                I::Let {
                    dst,
                    tag: *tag,
                    fields,
                    reuse,
                    rest: Box::new(self.stmt(rest, scope)),
                }
            }
            L::Clean(t, rest) => {
                let s = scope.at(*t);
                I::Clean(s, Box::new(self.stmt(rest, scope)))
            }
            L::DropReuse {
                name,
                fields,
                token,
                rest,
            } => {
                let src = scope.at(*name);
                let fields = fields.iter().map(|f| self.desc(*f, scope)).collect();
                let token = scope.at(*token);
                I::DropReuse {
                    src,
                    fields,
                    token,
                    rest: Box::new(self.stmt(rest, scope)),
                }
            }
            L::Switch {
                scrutinee,
                arms,
                default,
                keep_default,
            } => {
                let scrutinee = scope.at(*scrutinee);
                let mut out = Vec::with_capacity(arms.len());
                for a in arms {
                    // Bound before they are described: a field may be the
                    // descriptor another is described by.
                    for f in &a.fields {
                        scope.at(*f);
                    }
                    let fields = self.uses(&a.fields, scope);
                    let reuse = a.reuse.map(|t| scope.at(t));
                    out.push(Arm {
                        tag: u64::from(a.tag),
                        fields,
                        keep: a.keep,
                        reuse,
                        body: self.stmt(&a.body, scope),
                    });
                }
                I::Switch {
                    scrutinee,
                    arms: out,
                    default: Box::new(self.stmt(default, scope)),
                    keep_default: *keep_default,
                }
            }
            L::New {
                name,
                captures,
                methods,
                frame,
                rest,
            } => {
                let caps = self.uses(captures, scope);
                let (base, procs) = match self.news.get(&(l as *const L as usize)) {
                    Some(had) => had.clone(),
                    None => {
                        // The table's places for these methods, together,
                        // before any method made inside them takes the next.
                        let base = self.methods.len();
                        self.methods
                            .extend(std::iter::repeat_n((0, 0), methods.len()));
                        let mut procs = Vec::with_capacity(methods.len());
                        for (i, m) in methods.iter().enumerate() {
                            let kind = if *frame { "frame" } else { "method" };
                            let name = format!("{}/{kind}{i}", self.naming);
                            let p = self.proc(m, name);
                            let p = self.add_proc(p);
                            self.methods[base + i] = (p, captures.len());
                            procs.push(p);
                        }
                        let made = (base as u32, procs);
                        self.news.insert(l as *const L as usize, made.clone());
                        made
                    }
                };
                let one_result =
                    methods.len() == 1 && methods[0].params.len() == captures.len() + 1;
                let holds = |m: &LBlock| -> Vec<Hold> {
                    (0..captures.len())
                        .map(|i| {
                            if m.borrows(i) {
                                Hold::Borrowed
                            } else if linear::is_token(m.params[i]) {
                                Hold::Token
                            } else {
                                Hold::Owned
                            }
                        })
                        .collect()
                };
                if *frame && one_result && direct_ahead(rest, *name, captures) {
                    // Nothing made: the call ahead takes the frame's code
                    // where it is (see `direct`).
                    scope.pending.insert(
                        *name,
                        Pending {
                            method: &methods[0],
                            captures: caps,
                            holds: holds(&methods[0]),
                        },
                    );
                    return self.stmt(rest, scope);
                }
                let frame = (*frame && one_result).then(|| (procs[0], holds(&methods[0])));
                if frame.is_some() {
                    scope.frames.insert(*name);
                }
                let dst = scope.at(*name);
                I::New {
                    dst,
                    captures: caps,
                    base,
                    frame,
                    rest: Box::new(self.stmt(rest, scope)),
                }
            }
            L::Jump { label, args }
                if args.last().is_some_and(|a| scope.pending.contains_key(a))
                    && self.labels.contains_key(label) =>
            {
                let callee = Callee::Proc(self.labels[label]);
                self.direct(callee, args, scope)
            }
            L::Invoke { target, tag, args }
                if args.last().is_some_and(|a| scope.pending.contains_key(a)) =>
            {
                let callee = Callee::Method {
                    target: scope.at(*target),
                    tag: *tag,
                };
                self.direct(callee, args, scope)
            }
            L::Jump { label, args } => match self.labels.get(label) {
                Some(p) => I::Jump {
                    proc: *p,
                    args: args.iter().map(|a| scope.at(*a)).collect(),
                },
                None => I::Error(leak(format!("a jump to {label:?}, which is not compiled"))),
            },
            L::Invoke { target, tag, args } => I::Invoke {
                target: scope.at(*target),
                tag: *tag,
                args: args.iter().map(|a| scope.at(*a)).collect(),
            },
            L::Extern { op, args, blocks } => self.extern_(op, args, blocks, scope),
            L::Error(msg) => I::Error(msg),
            L::Mark(loc, rest) => I::Mark(*loc, Box::new(self.stmt(rest, scope))),
        }
    }

    /// A call whose continuation, last of `args`, is a pending frame: the
    /// callee called natively, and the frame's code compiled here, its
    /// parameters its captures' slots and then the answer's.
    fn direct(&mut self, callee: Callee, args: &[Name], scope: &mut Scope) -> I {
        let (k, args) = args.split_last().expect("a continuation");
        let pending = scope.pending.remove(k).expect("pending");
        let args = args.iter().map(|a| scope.at(*a)).collect();
        let m = pending.method;
        let n = pending.captures.len();
        for (p, c) in m.params.iter().zip(&pending.captures) {
            scope.slots.insert(*p, c.s);
        }
        let answer = scope.at(m.params[n]);
        I::Call {
            callee,
            args,
            held: pending.captures.into_iter().zip(pending.holds).collect(),
            answer,
            then: Box::new(self.stmt(&m.body, scope)),
        }
    }

    fn literal(&mut self, l: &Lit) -> K {
        K::W(match l {
            Lit::Int(n) | Lit::AnyInt(n, _) => *n as Word,
            Lit::Float(x) | Lit::AnyFloat(x, _) => x.to_bits(),
            Lit::Float32(x) => u64::from(x.to_bits()),
            Lit::Word(_, bits) => *bits,
            Lit::Char(c) => u64::from(*c as u32),
            Lit::Bool(b) => u64::from(*b),
            Lit::Unit => 0,
            Lit::Sym(s) => self.sym(*s) as Word,
            Lit::Str(s) => {
                return K::Text(Box::leak(s.as_bytes().to_vec().into_boxed_slice()));
            }
            Lit::BigInt(n) => return K::Big(*n),
        })
    }

    /// How primitive `p` is done, given its arguments and what it binds: in
    /// the order `meadow_llvm::emit` tries them.
    fn kind(&self, p: Prim, args: &[Name], result: Option<Name>, scope: &Scope) -> PK {
        if p == Prim::ThreadSpawn {
            // A thread's function answers what the thread does, and the
            // runtime keeps that answer for `await`: it is told how it is
            // represented.
            return PK::Spawn(match result.and_then(|r| self.program.threads.get(&r)) {
                Some(Rep::Var(d)) if *d != NO_DESC => match scope.slots.get(&VarId(*d)) {
                    Some(s) => D::S(*s),
                    None => D::K(desc::ANY),
                },
                Some(rep) => D::K(rep.desc().unwrap_or(desc::ANY)),
                None => D::K(desc::ANY),
            });
        }
        let left = args.first().and_then(|a| self.rep(*a));
        if args.len() == 2 && inline2(p, left) {
            return PK::Inline(untyped(p), left == Some(Rep::Float));
        }
        if p == Prim::Neg && args.len() == 1 && left == Some(Rep::Int) {
            return PK::Neg;
        }
        if direct(p) {
            return PK::Direct;
        }
        PK::Generic
    }

    /// A binary primitive's kind, on a left operand of `left`: inline, or the
    /// runtime's.
    fn kind2(&self, p: Prim, left: Name) -> PK {
        let rep = self.rep(left);
        if inline2(p, rep) {
            PK::Inline(untyped(p), rep == Some(Rep::Float))
        } else {
            PK::Generic
        }
    }

    fn extern_(
        &mut self,
        op: &'static Extern,
        args: &'static [Name],
        blocks: &'static [(Vec<Name>, L)],
        scope: &mut Scope,
    ) -> I {
        let uses = self.uses(args, scope);
        if op.is_branch() {
            let test = match op {
                Extern::Branch => Test::Truth,
                Extern::BranchPrim(p) => Test::Prim(*p, self.kind2(*p, args[0])),
                Extern::BranchPrimK(p, lit) => {
                    let k = self.literal(lit);
                    Test::PrimK(*p, self.kind2(*p, args[0]), k, lit_desc(lit))
                }
                _ => unreachable!("is_branch"),
            };
            let [(_, no), (_, yes)] = blocks else {
                return I::Error("a branch needs two continuations");
            };
            match test {
                Test::Prim(_, PK::Inline(q, f)) => {
                    if let Some(op) = B::of(q, f) {
                        return I::IfBin {
                            op,
                            a: uses[0].s,
                            b: uses[1].s,
                            no: Box::new(self.stmt(no, scope)),
                            yes: Box::new(self.stmt(yes, scope)),
                        };
                    }
                }
                Test::PrimK(_, PK::Inline(q, f), K::W(k), _) => {
                    if let Some(op) = B::of(q, f) {
                        return I::IfBinK {
                            op,
                            a: uses[0].s,
                            k,
                            no: Box::new(self.stmt(no, scope)),
                            yes: Box::new(self.stmt(yes, scope)),
                        };
                    }
                }
                _ => {}
            }
            return I::Branch {
                test,
                args: uses,
                no: Box::new(self.stmt(no, scope)),
                yes: Box::new(self.stmt(yes, scope)),
            };
        }
        let [(results, body)] = blocks else {
            return I::Error("a primitive that answers needs one continuation");
        };
        if let Extern::Prim(p @ (Prim::Enter | Prim::Detach | Prim::Reattach)) = op {
            let prim = *p;
            if prim == Prim::Detach {
                self.detaches = true;
            }
            // The code after it, as a closure's one method: what it uses but
            // does not bind are its captures, which it owns -- `linear`
            // shared what it borrowed before packing it.
            let key = body as *const L as usize;
            let (base, caps) = match self.codes.get(&key) {
                Some(had) => had.clone(),
                None => {
                    let caps: Vec<Name> = linear::free(body)
                        .into_iter()
                        .filter(|n| !results.contains(n))
                        .collect();
                    let mut params = caps.clone();
                    params.extend(results.iter().copied());
                    let code: &'static LBlock = Box::leak(Box::new(LBlock {
                        params,
                        borrowed: Vec::new(),
                        body: body.clone(),
                    }));
                    let base = self.methods.len();
                    self.methods.push((0, 0));
                    let name = format!("{}/after{base}", self.naming);
                    let compiled = self.proc(code, name);
                    let compiled = self.add_proc(compiled);
                    self.methods[base] = (compiled, caps.len());
                    self.codes.insert(key, (base as u32, caps.clone()));
                    (base as u32, caps)
                }
            };
            return I::Segment {
                prim,
                arg: uses[0],
                base,
                captures: self.uses(&caps, scope),
            };
        }
        let result = results.first().copied();
        let op = match op {
            Extern::Lit(l) => Op::Lit(self.literal(l)),
            Extern::Prim(p) => {
                if *p == Prim::ThreadSpawn {
                    self.threads = true;
                }
                Op::Prim(*p, self.kind(*p, args, result, scope))
            }
            Extern::PrimK(p, lit) => {
                let k = self.literal(lit);
                Op::PrimK(*p, self.kind2(*p, args[0]), k, lit_desc(lit))
            }
            Extern::Field(i) => Op::Field(*i as i64),
            Extern::Array => Op::Array,
            // A record keeps its labels in one order, the interned names'.
            Extern::Record(ls) => {
                let mut order: Vec<usize> = (0..ls.len()).collect();
                order.sort_by_key(|i| ls[*i]);
                let labels = order.iter().map(|i| self.sym(ls[*i])).collect();
                Op::Record(order, labels)
            }
            // A record keeps label and value in turn, in the labels' order: a
            // label whose place the type says has its value at `2 at + 1`.
            Extern::Select(_, Some(place)) => Op::SelectAt(2 * i64::from(place.at) + 1),
            Extern::Select(label, None) => Op::Select(self.sym(*label)),
            Extern::Extend(_, Some(p)) => Op::Update(i64::from(p.at), i64::from(p.of)),
            Extern::Extend(label, None) => Op::Extend(self.sym(*label)),
            Extern::Native(e, o) => {
                let c = |s: &str| -> &'static CStr {
                    Box::leak(CString::new(s).unwrap_or_default().into_boxed_c_str())
                };
                Op::Native(c(e), c(o))
            }
            other => return I::Error(leak(format!("the interpreter cannot do {other:?}"))),
        };
        let result = result.map(|r| scope.at(r));
        if let Some(dst) = result {
            match op {
                Op::Prim(_, PK::Inline(q, f)) => {
                    if let Some(b) = B::of(q, f) {
                        return I::Bin {
                            op: b,
                            a: uses[0].s,
                            b: uses[1].s,
                            dst,
                            rest: Box::new(self.stmt(body, scope)),
                        };
                    }
                }
                Op::PrimK(_, PK::Inline(q, f), K::W(k), _) => {
                    if let Some(b) = B::of(q, f) {
                        return I::BinK {
                            op: b,
                            a: uses[0].s,
                            k,
                            dst,
                            rest: Box::new(self.stmt(body, scope)),
                        };
                    }
                }
                _ => {}
            }
        }
        I::Prim {
            op,
            args: uses,
            result,
            rest: Box::new(self.stmt(body, scope)),
        }
    }
}

/// Whether the frame `k` made of `captures` can be left unmade: whether
/// the statements after it reach, without branching, a call that takes it
/// as its continuation and uses it for nothing else, and without touching it
/// or what it captured -- which stay where they are for the call to take --
/// on the way.
fn direct_ahead(mut l: &L, k: Name, captures: &[Name]) -> bool {
    let mut ks = vec![k];
    let mine = |n: &Name, ks: &[Name]| ks.contains(n) || captures.contains(n);
    loop {
        match l {
            L::Mark(_, rest) => l = rest,
            L::Share(n, rest) => {
                if ks.contains(n) {
                    return false;
                }
                l = rest;
            }
            L::Erase(n, rest) | L::Clean(n, rest) => {
                if mine(n, &ks) {
                    return false;
                }
                l = rest;
            }
            L::Rename(binds, rest) => {
                for (to, from) in binds {
                    if to == from {
                        continue;
                    }
                    if ks.contains(from) {
                        ks.push(*to);
                    } else if mine(from, &ks) || mine(to, &ks) {
                        return false;
                    }
                }
                l = rest;
            }
            L::Let {
                name,
                fields,
                reuse,
                rest,
                ..
            } => {
                if mine(name, &ks)
                    || fields.iter().any(|f| ks.contains(f))
                    || reuse.is_some_and(|r| mine(&r, &ks))
                {
                    return false;
                }
                l = rest;
            }
            L::Extern { op, args, blocks } => {
                let [(results, body)] = blocks.as_slice() else {
                    return false;
                };
                if op.is_branch()
                    || matches!(
                        op,
                        Extern::Prim(Prim::Enter | Prim::Detach | Prim::Reattach)
                    )
                    || args.iter().any(|a| ks.contains(a))
                    || results.iter().any(|r| mine(r, &ks))
                {
                    return false;
                }
                l = body;
            }
            L::Jump { args, .. } => return ends_with_one(args, &ks),
            L::Invoke { target, args, .. } => {
                return !ks.contains(target) && ends_with_one(args, &ks);
            }
            _ => return false,
        }
    }
}

/// Whether `args` ends with one of `ks` and has none of them elsewhere.
fn ends_with_one(args: &[Name], ks: &[Name]) -> bool {
    match args.split_last() {
        Some((last, rest)) => ks.contains(last) && !rest.iter().any(|a| ks.contains(a)),
        None => false,
    }
}

/// The primitives with an entry of their own in the runtime: those a program
/// does millions of times, and those that consume their array.
fn direct(p: Prim) -> bool {
    matches!(
        p,
        Prim::Hash
            | Prim::Eq
            | Prim::StringIndexOf
            | Prim::StringSlice
            | Prim::StSetArray
            | Prim::GlobalReady
            | Prim::GlobalGet
            | Prim::ArrayPush
            | Prim::ArrayConcat
    )
}

// --- running --------------------------------------------------------------------

impl Vm {
    /// How many tests the program is, or `None` if it is a program.
    pub fn tests(&self) -> Option<usize> {
        self.is_tests.then_some(self.entries.len())
    }

    /// The entry point, given the null continuation: what it answers.
    pub fn run_entry(&self) -> Word {
        let i = CHOSEN.load(Ordering::Relaxed);
        let mut args = Args::new();
        args.push(V::Val(0));
        self.call(self.entries[i], args)
    }

    /// Method `tag` of the closure `obj`, with `args`: entered from the
    /// runtime, as the compiled code's `meadow_invoke1` and `meadow_apply`.
    pub fn invoke(&self, obj: Word, tag: u32, args: &[Word]) -> Word {
        let (p, ncap) = self.methods[method_of(obj) + tag as usize];
        let env = self.enter_method(obj, p, ncap, args.iter().map(|a| V::Val(*a)).collect());
        self.safe_point(p, &env);
        self.exec(p, env)
    }

    /// Procedure `p` with `args`, to its native return.
    fn call(&self, p: u32, args: Args) -> Word {
        let mut env = Env::new();
        self.bind(p, args, &mut env);
        self.safe_point(p, &env);
        self.exec(p, env)
    }

    /// `env` made procedure `p`'s, holding `args` as its first slots.
    fn bind(&self, p: u32, args: Args, env: &mut Env) {
        env.clear();
        env.resize_with(self.procs[p as usize].slots.max(args.len()), V::default);
        for (slot, a) in env.iter_mut().zip(args) {
            *slot = a;
        }
    }

    fn exec(&self, p: u32, env: Env) -> Word {
        // Every call the interpreter makes natively begins here: where a
        // growable program checks it has stack enough left.
        if self.growable && meadow_silo::stack::low() {
            return meadow_silo::stack::grow(move || self.exec(p, env));
        }
        if self.debugging {
            self.run::<true>(p, env)
        } else {
            self.run::<false>(p, env)
        }
    }

    /// The interpreter's loop, from the start of procedure `p`: with a
    /// debugger's record kept and its stops checked when `DEBUG`, which a
    /// program run without one never pays for.
    fn run<const DEBUG: bool>(&self, mut p: u32, mut env: Env) -> Word {
        let code = &self.code;
        let mut pc = self.procs[p as usize].entry as usize;
        // The record a debugger reads this activation by, on the running
        // segment's chain while it runs.
        let parent = if DEBUG { meadow_silo::frames::top() } else { 0 };
        let mut act = debug::Act {
            parent,
            depth: debug::Act::depth_of(parent) + 1,
            proc: p,
            pc,
            env: &env as *const Env,
        };
        let _popped = DEBUG.then(|| {
            meadow_silo::frames::set_top(&act as *const debug::Act as Word);
            Popped(parent)
        });
        let watching = if DEBUG { debug::debugger() } else { None };
        loop {
            if DEBUG && let Some(w) = watching {
                act.proc = p;
                act.pc = pc;
                w.at(&act);
            }
            // Safety: every pc is a procedure's entry, an instruction's
            // successor within its procedure, or a target `Code::emit`
            // patched in -- all inside the array.
            match unsafe { *code.ins.get_unchecked(pc) } {
                Ins::Share { s, d: dd } => {
                    let v = self.val(s.into(), &mut env);
                    self.share(v, d(dd.get(), &env));
                    pc += 1;
                }
                Ins::Erase { s, d: dd } => {
                    self.erase_slot(s.into(), dd.get(), &mut env);
                    pc += 1;
                }
                Ins::Rename(i) => {
                    let moved: SmallVec<[(Slot, V); 4]> = code.renames[i as usize]
                        .iter()
                        .map(|(to, from)| (*to, take_val(&mut env, *from)))
                        .collect();
                    for (to, v) in moved {
                        env[to as usize] = v;
                    }
                    pc += 1;
                }
                Ins::Let(i) => {
                    let l = &code.lets[i as usize];
                    let vals: Words = l.fields.iter().map(|u| self.val(u.s, &mut env)).collect();
                    let descs: Descs = l.fields.iter().map(|u| d(u.d, &env)).collect();
                    let token = l.reuse.map(|t| {
                        let v = self.val(t, &mut env);
                        env[t as usize] = V::Empty;
                        v
                    });
                    let v = self.build_in(l.tag, &vals, &descs, token);
                    env[l.dst as usize] = V::Val(v);
                    pc += 1;
                }
                Ins::Clean(t) => {
                    let tok = self.val(t, &mut env);
                    self.clean_token(tok);
                    env[t as usize] = V::Empty;
                    pc += 1;
                }
                Ins::DropReuse(i) => {
                    let x = &code.drops[i as usize];
                    let v = self.val(x.src, &mut env);
                    let tok = self.drop_reuse(v, &x.fields, &env);
                    env[x.src as usize] = V::Empty;
                    env[x.token as usize] = V::Val(tok);
                    pc += 1;
                }
                Ins::Switch(i) => {
                    let sw = &code.switches[i as usize];
                    let v = self.val(sw.scrutinee, &mut env);
                    let tag = if v & 1 == 1 {
                        v >> 1
                    } else {
                        u64::from(heap::meta(v))
                    };
                    match sw.arms.iter().find(|a| a.tag == tag) {
                        Some(a) => {
                            let loaded = load_fields(v, a.fields.len());
                            for (u, x) in a.fields.iter().zip(&loaded) {
                                env[u.s as usize] = V::Val(*x);
                            }
                            if !a.keep {
                                let descs: Descs = a.fields.iter().map(|u| d(u.d, &env)).collect();
                                match a.reuse {
                                    Some(t) => {
                                        let tok = self.release_reuse(v, &loaded, &descs);
                                        env[t as usize] = V::Val(tok);
                                    }
                                    None => self.release(v, &loaded, &descs),
                                }
                            }
                            pc = a.pc as usize;
                        }
                        None => {
                            if !sw.keep_default {
                                self.erase(v, desc::REF);
                            }
                            pc = sw.default as usize;
                        }
                    }
                }
                Ins::New(i) => {
                    let x = &code.news[i as usize];
                    let v = match &x.frame {
                        Some((fp, holds)) => {
                            let mut fr = new_frame(*fp, x.base);
                            for (u, h) in x.captures.iter().zip(holds) {
                                let dd = d(u.d, &env);
                                fr.captured.push((take_val(&mut env, u.s), dd, *h));
                            }
                            V::Frame(fr)
                        }
                        None => {
                            let vals: Words =
                                x.captures.iter().map(|u| self.val(u.s, &mut env)).collect();
                            let descs: Descs = x.captures.iter().map(|u| d(u.d, &env)).collect();
                            V::Val(build_closure(x.base, &vals, &descs))
                        }
                    };
                    env[x.dst as usize] = v;
                    pc += 1;
                }
                Ins::Jump(i) => {
                    let j = &code.jumps[i as usize];
                    let args = &j.args;
                    if !args.iter().any(|a| matches!(env[*a as usize], V::Frame(_))) {
                        // No frame among them: the words, into the frame of
                        // words this procedure's slots reuse. What is left in
                        // the others is never read before it is written.
                        let words: Words = args.iter().map(|a| self.word(*a, &mut env)).collect();
                        p = j.proc;
                        let need = self.procs[p as usize].slots.max(words.len());
                        if env.len() < need {
                            env.resize_with(need, V::default);
                        }
                        for (slot, w) in env.iter_mut().zip(words) {
                            *slot = V::Val(w);
                        }
                        self.safe_point(p, &env);
                        pc = self.procs[p as usize].entry as usize;
                        continue;
                    }
                    let (vals, frame) = self.transfer(args, &mut env);
                    match frame {
                        None => {
                            p = j.proc;
                            self.bind(p, vals, &mut env);
                            self.safe_point(p, &env);
                        }
                        Some(fr) => {
                            let callee = j.proc;
                            let r = self.waiting(&fr, || self.call(callee, vals));
                            p = self.enter_frame(fr, V::Val(r), &mut env);
                        }
                    }
                    pc = self.procs[p as usize].entry as usize;
                }
                Ins::Invoke(i) => {
                    let iv = &code.invokes[i as usize];
                    let (target, tag, args) = (iv.target, iv.tag, &iv.args);
                    if let V::Frame(_) = &env[target as usize] {
                        // Our own frame, entered directly: its code, here.
                        let V::Frame(fr) = std::mem::take(&mut env[target as usize]) else {
                            unreachable!()
                        };
                        let [a] = args.as_slice() else {
                            fail("a frame takes one answer")
                        };
                        let a = take_val(&mut env, *a);
                        p = self.enter_frame(fr, a, &mut env);
                        pc = self.procs[p as usize].entry as usize;
                        continue;
                    }
                    let t = self.val(target, &mut env);
                    // A continuation of one argument may be the null one:
                    // return natively.
                    if tag == 0 && args.len() == 1 && t == 0 {
                        return self.val(args[0], &mut env);
                    }
                    let (m, ncap) = self.methods[method_of(t) + tag as usize];
                    let (vals, frame) = self.transfer(args, &mut env);
                    match frame {
                        None => {
                            p = m;
                            env = self.enter_method(t, m, ncap, vals);
                            self.safe_point(p, &env);
                        }
                        Some(fr) => {
                            let r = self.waiting(&fr, || {
                                let e = self.enter_method(t, m, ncap, vals);
                                self.safe_point(m, &e);
                                self.exec(m, e)
                            });
                            p = self.enter_frame(fr, V::Val(r), &mut env);
                        }
                    }
                    pc = self.procs[p as usize].entry as usize;
                }
                Ins::Call(i) => {
                    let c = &code.calls[i as usize];
                    let mut vals: Args = c
                        .args
                        .iter()
                        .map(|a| V::Val(self.val(*a, &mut env)))
                        .collect();
                    vals.push(V::Val(0));
                    let r = match c.callee {
                        Callee::Proc(q) => self.waiting_in(&c.held, &env, || self.call(q, vals)),
                        Callee::Method { target, tag } => {
                            let t = self.val(target, &mut env);
                            if t == 0 {
                                fail("the null continuation was given a frame");
                            }
                            let (m, ncap) = self.methods[method_of(t) + tag as usize];
                            self.waiting_in(&c.held, &env, || {
                                let e = self.enter_method(t, m, ncap, vals);
                                self.safe_point(m, &e);
                                self.exec(m, e)
                            })
                        }
                    };
                    env[c.answer as usize] = V::Val(r);
                    pc += 1;
                }
                Ins::Prim(i) => {
                    let x = &code.prims[i as usize];
                    let v = self.op(&x.op, &x.args, &mut env);
                    if let Some(r) = x.result {
                        env[r as usize] = V::Val(v);
                    }
                    pc += 1;
                }
                Ins::Bin { op, a, b, dst } => {
                    let r = op.run(self.word(a.into(), &mut env), self.word(b.into(), &mut env));
                    env[dst as usize] = V::Val(r);
                    pc += 1;
                }
                Ins::BinK { op, a, dst, k } => {
                    let r = op.run(self.word(a.into(), &mut env), code.consts[k as usize]);
                    env[dst as usize] = V::Val(r);
                    pc += 1;
                }
                Ins::BinI { op, a, dst, k } => {
                    let r = op.run(self.word(a.into(), &mut env), k as i64 as Word);
                    env[dst as usize] = V::Val(r);
                    pc += 1;
                }
                Ins::IfBin { op, a, b, no } => {
                    let r = op.run(self.word(a.into(), &mut env), self.word(b.into(), &mut env));
                    pc = if r != 0 { pc + 1 } else { no as usize };
                }
                Ins::IfBinI { op, a, k, no } => {
                    let r = op.run(self.word(a.into(), &mut env), k as i64 as Word);
                    pc = if r != 0 { pc + 1 } else { no as usize };
                }
                Ins::Branch(t) => {
                    let t = &code.tests[t as usize];
                    pc = if self.test(&t.test, &t.args, &mut env) {
                        pc + 1
                    } else {
                        t.no as usize
                    };
                }
                Ins::Wide(w) => match code.wides[w as usize] {
                    WideI::Share { s, d: dd } => {
                        let v = self.val(s, &mut env);
                        self.share(v, d(dd, &env));
                        pc += 1;
                    }
                    WideI::Erase { s, d: dd } => {
                        self.erase_slot(s, dd, &mut env);
                        pc += 1;
                    }
                    WideI::Bin { op, a, b, dst } => {
                        let r = op.run(self.word(a, &mut env), self.word(b, &mut env));
                        env[dst as usize] = V::Val(r);
                        pc += 1;
                    }
                    WideI::BinK { op, a, k, dst } => {
                        let r = op.run(self.word(a, &mut env), k);
                        env[dst as usize] = V::Val(r);
                        pc += 1;
                    }
                    WideI::IfBin { op, a, b, no } => {
                        let r = op.run(self.word(a, &mut env), self.word(b, &mut env));
                        pc = if r != 0 { pc + 1 } else { no as usize };
                    }
                    WideI::IfBinK { op, a, k, no } => {
                        let r = op.run(self.word(a, &mut env), k);
                        pc = if r != 0 { pc + 1 } else { no as usize };
                    }
                },
                Ins::Segment(i) => {
                    let sg = &code.segments[i as usize];
                    let vals: Words = sg
                        .captures
                        .iter()
                        .map(|u| self.val(u.s, &mut env))
                        .collect();
                    let descs: Descs = sg.captures.iter().map(|u| d(u.d, &env)).collect();
                    let packed = build_closure(sg.base, &vals, &descs);
                    let a = self.val(sg.arg.s, &mut env);
                    return match sg.prim {
                        Prim::Enter => meadow_silo::segments::meadow_enter(a, packed),
                        Prim::Detach => meadow_silo::segments::meadow_detach(a, packed),
                        _ => meadow_silo::segments::meadow_reattach(a, packed),
                    };
                }
                Ins::Error(i) => fail(code.errors[i as usize]),
            }
        }
    }

    /// The whole program's code, procedure by procedure, as `--dis` writes it.
    pub fn disassemble(&self) -> String {
        use std::fmt::Write;
        let mut order: Vec<usize> = (0..self.procs.len()).collect();
        order.sort_by_key(|i| self.procs[*i].entry);
        let mut out = String::new();
        for (k, i) in order.iter().enumerate() {
            let pr = &self.procs[*i];
            let end = order
                .get(k + 1)
                .map_or(self.code.ins.len(), |j| self.procs[*j].entry as usize);
            let _ = writeln!(
                out,
                "p{i} {} ({} registers, {} parameters)",
                pr.name,
                pr.slots,
                pr.params.len()
            );
            let mut last = None;
            for pc in pr.entry as usize..end {
                let loc = self.code.locs[pc];
                let at = if loc != last && loc.is_some() {
                    let l = loc.expect("some");
                    format!("  ; source {} at {}", l.source, l.span.start)
                } else {
                    String::new()
                };
                last = loc;
                let _ = writeln!(out, "  {pc:6}  {}{at}", self.code.show(pc));
            }
        }
        out
    }

    /// Erase what slot `s` holds, described by `dd`: a word, or a frame
    /// nobody will call.
    #[inline(always)]
    fn erase_slot(&self, s: Slot, dd: D, env: &mut Env) {
        match std::mem::take(&mut env[s as usize]) {
            V::Frame(fr) => {
                self.erase_frame(&fr);
                recycle(fr);
            }
            V::Val(v) => self.erase(v, d(dd, env)),
            V::Empty => fail("a name not in scope was erased"),
        }
    }

    /// The arguments of a call, as values: every frame among them built,
    /// but the last, which is the call's own continuation -- answered as
    /// the frame, with the null continuation in its place.
    fn transfer(&self, args: &[Slot], env: &mut [V]) -> (Args, Option<Box<Frame>>) {
        let native = args
            .iter()
            .rposition(|a| matches!(env[*a as usize], V::Frame(_)));
        let mut vals = Args::with_capacity(args.len());
        let mut frame = None;
        for (i, a) in args.iter().enumerate() {
            if Some(i) == native {
                if let V::Frame(fr) = std::mem::take(&mut env[*a as usize]) {
                    frame = Some(fr);
                }
                vals.push(V::Val(0));
            } else {
                vals.push(V::Val(self.val(*a, env)));
            }
        }
        (vals, frame)
    }

    /// A frame's code, entered with its captures and `answer`: `env` made its.
    fn enter_frame(&self, mut fr: Box<Frame>, answer: V, env: &mut Env) -> u32 {
        let mut args: Args = fr.captured.drain(..).map(|(v, _, _)| v).collect();
        args.push(answer);
        let p = fr.proc;
        recycle(fr);
        self.bind(p, args, env);
        p
    }

    /// Run `call`, which `fr` is waiting on: with what `fr` holds on the
    /// shadow chain while it runs, where a discarded continuation or a
    /// collection can find it (`meadow_silo::shadow`).
    fn waiting(&self, fr: &Frame, call: impl FnOnce() -> Word) -> Word {
        if !self.shadow {
            return call();
        }
        let prev = meadow_silo::shadow::head();
        let mut rec: SmallVec<[i64; 18]> = SmallVec::new();
        rec.push(prev as i64);
        rec.push(0);
        held(fr, &mut rec);
        if rec.len() == 2 {
            return call();
        }
        rec[1] = (rec.len() as i64 - 2) / 2;
        meadow_silo::shadow::set(rec.as_ptr() as Word);
        let r = call();
        meadow_silo::shadow::set(prev);
        drop(rec);
        r
    }

    /// [`Vm::waiting`], for a frame never made: what it would have held is in
    /// `env`, where `held` says.
    fn waiting_in(&self, held: &[(U, Hold)], env: &[V], call: impl FnOnce() -> Word) -> Word {
        if !self.shadow {
            return call();
        }
        let prev = meadow_silo::shadow::head();
        let mut rec: SmallVec<[i64; 18]> = SmallVec::new();
        rec.push(prev as i64);
        rec.push(0);
        for (u, h) in held {
            hold(&env[u.s as usize], d(u.d, env), *h, &mut rec);
        }
        if rec.len() == 2 {
            return call();
        }
        rec[1] = (rec.len() as i64 - 2) / 2;
        meadow_silo::shadow::set(rec.as_ptr() as Word);
        let r = call();
        meadow_silo::shadow::set(prev);
        drop(rec);
        r
    }

    /// Under tracing, where a function begins: if a collection is due, run it
    /// with the parameters on the shadow chain.
    fn safe_point(&self, p: u32, env: &[V]) {
        if !self.tracing || meadow_silo::trace::meadow_gc_due.load(Ordering::Relaxed) == 0 {
            return;
        }
        let prev = meadow_silo::shadow::head();
        let mut rec: Vec<i64> = vec![prev as i64, 0];
        for (i, pd) in self.procs[p as usize].params.iter().enumerate() {
            if let V::Val(v) = env[i] {
                let dd = d(*pd, env);
                if dd == desc::REF {
                    rec.push(v as i64);
                    rec.push(dd);
                }
            }
        }
        rec[1] = (rec.len() as i64 - 2) / 2;
        meadow_silo::shadow::set(rec.as_ptr() as Word);
        meadow_silo::trace::meadow_gc_poll(meadow_silo::ctx::get());
        meadow_silo::shadow::set(prev);
    }

    /// A method's entry: its captures loaded out of the closure `obj`, which
    /// is released, and then its arguments.
    fn enter_method(&self, obj: Word, p: u32, ncap: usize, args: Args) -> Env {
        let proc = &self.procs[p as usize];
        let mut env = Env::new();
        env.resize_with(proc.slots.max(ncap + args.len()), V::default);
        let loaded = load_fields(obj, ncap);
        for (slot, x) in env.iter_mut().zip(&loaded) {
            *slot = V::Val(*x);
        }
        for (slot, a) in env[ncap..].iter_mut().zip(args) {
            *slot = a;
        }
        let descs: Descs = proc.params[..ncap].iter().map(|pd| d(*pd, &env)).collect();
        self.release(obj, &loaded, &descs);
        env
    }

    // --- values ---------------------------------------------------------------

    /// Slot `s`'s word: [`Vm::val`], with the common case first.
    #[inline(always)]
    fn word(&self, s: Slot, env: &mut [V]) -> Word {
        match env[s as usize] {
            V::Val(v) => v,
            _ => self.val(s, env),
        }
    }

    /// Slot `s`'s word, building it if it is a frame not built.
    fn val(&self, s: Slot, env: &mut [V]) -> Word {
        match &env[s as usize] {
            V::Val(v) => *v,
            V::Frame(_) => {
                let V::Frame(fr) = std::mem::take(&mut env[s as usize]) else {
                    unreachable!()
                };
                let v = self.materialize(fr);
                env[s as usize] = V::Val(v);
                v
            }
            V::Empty => fail("a name not in scope was read"),
        }
    }

    /// Build a frame as the object it would have been.
    fn materialize(&self, mut fr: Box<Frame>) -> Word {
        if fr.captured.iter().any(|(_, _, h)| *h == Hold::Borrowed) {
            fail("a frame that borrows what it holds was built as an object");
        }
        let mut vals = Words::new();
        let mut descs = Descs::new();
        for (c, dd, _) in fr.captured.drain(..) {
            vals.push(match c {
                V::Val(v) => v,
                V::Frame(inner) => self.materialize(inner),
                V::Empty => fail("a frame captured a name not in scope"),
            });
            descs.push(dd);
        }
        let base = fr.base;
        recycle(fr);
        build_closure(base, &vals, &descs)
    }

    // --- counting ---------------------------------------------------------------

    fn share(&self, v: Word, d: i64) {
        if !self.tracing {
            heap::share(v, d);
        }
    }

    fn erase(&self, v: Word, d: i64) {
        if !self.tracing {
            heap::erase(v, d);
        }
    }

    /// The paper's `release`: `v`'s fields, `loaded`, are ours now. If `v`
    /// was the last reference its block is clean at once and no count moves;
    /// otherwise its count goes down and each field is shared.
    fn release(&self, v: Word, loaded: &[Word], descs: &[i64]) {
        if self.tracing {
            return;
        }
        if loaded.is_empty() {
            self.erase(v, desc::REF);
            return;
        }
        let rc = count(v);
        if rc == 0 {
            heap::clean(v);
            return;
        }
        set_count(v, rc - 1);
        for (x, dd) in loaded.iter().zip(descs) {
            self.share(*x, *dd);
        }
        if rc > heap::REGION_FLOOR {
            meadow_silo::region::meadow_region_erased(v);
        }
    }

    /// [`Vm::release`], keeping the block as a reuse token when this was the
    /// last reference; `0` when it was not.
    fn release_reuse(&self, v: Word, loaded: &[Word], descs: &[i64]) -> Word {
        if self.tracing {
            return 0;
        }
        let rc = count(v);
        if rc == 0 {
            return v;
        }
        set_count(v, rc - 1);
        for (x, dd) in loaded.iter().zip(descs) {
            self.share(*x, *dd);
        }
        if rc > heap::REGION_FLOOR {
            meadow_silo::region::meadow_region_erased(v);
        }
        0
    }

    /// Erase `v`, keeping its block as a reuse token when this was the last
    /// reference -- its references to its fields given up -- and `0` when not.
    fn drop_reuse(&self, v: Word, fields: &[D], env: &[V]) -> Word {
        if self.tracing {
            return 0;
        }
        if heap::is_block(v) && count(v) == 0 {
            let loaded = load_fields(v, fields.len());
            for (x, fd) in loaded.iter().zip(fields) {
                self.erase(*x, d(*fd, env));
            }
            return v;
        }
        self.erase(v, desc::REF);
        0
    }

    fn clean_token(&self, token: Word) {
        if !self.tracing && token != 0 {
            heap::clean(token);
        }
    }

    /// A frame nobody will call: what it owns goes.
    fn erase_frame(&self, fr: &Frame) {
        if self.tracing {
            return;
        }
        for (c, dd, h) in &fr.captured {
            match (c, h) {
                (_, Hold::Borrowed) | (V::Empty, _) => {}
                (V::Frame(inner), _) => self.erase_frame(inner),
                (V::Val(v), Hold::Token) => self.clean_token(*v),
                (V::Val(v), _) => self.erase(*v, *dd),
            }
        }
    }

    // --- blocks ---------------------------------------------------------------

    /// A data block of `tag` holding `vals`: in the reuse token's block when
    /// it holds one, which is this size; a new one otherwise. No fields is no
    /// block.
    fn build_in(&self, tag: u32, vals: &[Word], descs: &[i64], token: Option<Word>) -> Word {
        if vals.is_empty() {
            return (u64::from(tag) << 1) | 1;
        }
        match token {
            Some(t) if t != 0 => {
                write_block(t, heap::DATA, tag, vals, descs);
                t
            }
            _ => heap::build(heap::DATA, tag, vals, descs),
        }
    }

    // --- primitives -----------------------------------------------------------

    fn op(&self, op: &Op, args: &[U], env: &mut [V]) -> Word {
        match op {
            Op::Lit(k) => literal(*k),
            Op::Prim(p, kind) => {
                let vals: Words = args.iter().map(|u| self.val(u.s, env)).collect();
                let descs: Descs = args.iter().map(|u| d(u.d, env)).collect();
                self.prim(*p, *kind, &vals, &descs, env)
            }
            Op::PrimK(p, kind, k, kd) => {
                let a = self.val(args[0].s, env);
                let k = literal(*k);
                let r = match kind {
                    PK::Inline(q, float) => inline(*q, *float, a, k),
                    _ => generic(*p, &[a, k], &[d(args[0].d, env), *kd]),
                };
                self.erase(k, *kd);
                r
            }
            Op::Field(i) => meadow_silo::meadow_field(self.val(args[0].s, env), *i),
            Op::SelectAt(at) => meadow_silo::meadow_field(self.val(args[0].s, env), *at),
            Op::Select(sym) => meadow_silo::prims::meadow_select(self.val(args[0].s, env), *sym),
            Op::Update(at, of) => {
                let rec = self.val(args[0].s, env);
                let v = self.val(args[1].s, env);
                meadow_silo::prims::meadow_update(rec, *at, *of, v, d(args[1].d, env))
            }
            Op::Extend(sym) => {
                let rec = self.val(args[0].s, env);
                let v = self.val(args[1].s, env);
                meadow_silo::prims::meadow_extend(rec, *sym, v, d(args[1].d, env))
            }
            Op::Array => {
                let vals: Words = args.iter().map(|u| self.val(u.s, env)).collect();
                let descs: Descs = args.iter().map(|u| d(u.d, env)).collect();
                // Safety: arrays of `n` words each.
                unsafe {
                    meadow_silo::prims::meadow_array(
                        vals.len() as i64,
                        vals.as_ptr(),
                        descs.as_ptr(),
                    )
                }
            }
            Op::Record(order, labels) => {
                let vals: Words = order.iter().map(|i| self.val(args[*i].s, env)).collect();
                let descs: Descs = order.iter().map(|i| d(args[*i].d, env)).collect();
                // Safety: arrays of `n` words each.
                unsafe {
                    meadow_silo::prims::meadow_record(
                        vals.len() as i64,
                        vals.as_ptr(),
                        descs.as_ptr(),
                        labels.as_ptr(),
                    )
                }
            }
            Op::Native(e, o) => {
                let a = self.val(args[0].s, env);
                // Safety: NUL-terminated names.
                unsafe {
                    meadow_silo::native::meadow_native(e.as_ptr(), o.as_ptr(), a, d(args[0].d, env))
                }
            }
        }
    }

    /// A primitive, by the way the compiled code would do it.
    fn prim(&self, p: Prim, kind: PK, vals: &[Word], descs: &[i64], env: &[V]) -> Word {
        use meadow_silo::prims as rt;
        match kind {
            PK::Inline(q, float) => inline(q, float, vals[0], vals[1]),
            PK::Neg => (vals[0] as i64).wrapping_neg() as Word,
            PK::Spawn(answer) => {
                let mut ws: Words = vals.into();
                let mut ds: Descs = descs.into();
                ws.push(d(answer, env) as Word);
                ds.push(desc::INT);
                generic(p, &ws, &ds)
            }
            PK::Direct => match p {
                Prim::Hash => rt::meadow_hash(vals[0], descs[0]),
                Prim::Eq => rt::meadow_equal(vals[0], descs[0], vals[1], descs[1]),
                Prim::StringIndexOf => rt::meadow_string_index_of(vals[0], vals[1], vals[2]),
                Prim::StringSlice => rt::meadow_string_slice(vals[0], vals[1], vals[2]),
                Prim::StSetArray => rt::meadow_st_set(vals[0], vals[1], vals[2], descs[2]),
                Prim::GlobalReady => rt::meadow_global_ready(vals[0]),
                Prim::GlobalGet => rt::meadow_global_get(vals[0]),
                Prim::ArrayPush => rt::meadow_array_push(vals[0], vals[1], descs[1]),
                Prim::ArrayConcat => rt::meadow_array_concat(vals[0], vals[1]),
                _ => unreachable!("direct"),
            },
            PK::Generic => generic(p, vals, descs),
        }
    }

    fn test(&self, test: &Test, args: &[U], env: &mut [V]) -> bool {
        match test {
            Test::Truth => self.val(args[0].s, env) != 0,
            Test::Prim(p, kind) => {
                let (a, b) = (self.val(args[0].s, env), self.val(args[1].s, env));
                binary(*p, *kind, a, b, d(args[0].d, env)) != 0
            }
            Test::PrimK(p, kind, k, kd) => {
                let a = self.val(args[0].s, env);
                let k = literal(*k);
                let r = binary(*p, *kind, a, k, d(args[0].d, env));
                self.erase(k, *kd);
                r != 0
            }
        }
    }

    /// The tables the runtime prints and takes records apart by, as the
    /// compiler writes them (`meadow_llvm::emit`'s `header`).
    fn names(&self) -> meadow_silo::show::Names {
        let mut ctors: Vec<(u32, String)> = self
            .program
            .tags
            .iter()
            .map(|(n, t)| (*t, n.to_string()))
            .collect();
        ctors.sort();
        let count = ctors.last().map_or(0, |(t, _)| *t as usize + 1);
        let mut names = vec![None; count];
        let mut fields = vec![None; count];
        for (t, name) in &ctors {
            names[*t as usize] = Some(name.clone());
            if let Some(fs) = self
                .program
                .ctor_fields
                .get(&InternedString::from(name.as_str()))
            {
                fields[*t as usize] = Some(fs.iter().map(|f| f.to_string()).collect());
            }
        }
        let roles = meadow_rt::roles::Role::ALL
            .into_iter()
            .map(|r| self.program.role(r).map(|c| c.to_string()))
            .collect();
        let mut syms: Vec<(usize, InternedString)> =
            self.syms.iter().map(|(s, i)| (*i, *s)).collect();
        syms.sort_by_key(|(i, _)| *i);
        // Where each goes among a record's labels: the interned names' order.
        let mut by: Vec<usize> = (0..syms.len()).collect();
        by.sort_by_key(|i| syms[*i].1);
        let mut ranks = vec![0i64; syms.len()];
        for (r, i) in by.iter().enumerate() {
            ranks[*i] = r as i64;
        }
        meadow_silo::show::Names {
            ctors: names,
            roles,
            fields,
            syms: syms.iter().map(|(_, s)| s.to_string()).collect(),
            ranks,
        }
    }
}

// --- helpers ------------------------------------------------------------------

/// Puts a debugger's chain back as it was before an activation began, when
/// the activation ends -- however it ends.
struct Popped(Word);

impl Drop for Popped {
    fn drop(&mut self) {
        meadow_silo::frames::set_top(self.0);
    }
}

/// Frames given back, for the next non-tail call to have without asking the
/// allocator -- which many threads share, and the runtime keeps to two
/// arenas. One pool for each Meadow thread, in its context
/// (`meadow_silo::ctx::Ctx::engine`): not in a thread-local, since a thread
/// is resumed on another OS thread mid-call, and what was this OS thread's
/// pool when the call began would be another thread's by the time it
/// returns.
type Pool = Vec<Box<Frame>>;

/// This thread's pool. The context is this thread's alone while it runs, and
/// nothing holds the pool across a call.
fn pool() -> &'static mut Pool {
    // Safety: the running thread's context, used by it alone.
    let cx = unsafe { &mut *meadow_silo::ctx::get() };
    cx.engine
        .get_or_insert_with(|| Box::new(Pool::new()))
        .downcast_mut::<Pool>()
        .expect("the interpreter's pool")
}

fn new_frame(proc: u32, base: u32) -> Box<Frame> {
    match pool().pop() {
        Some(mut fr) => {
            fr.proc = proc;
            fr.base = base;
            fr
        }
        None => Box::new(Frame {
            proc,
            base,
            captured: SmallVec::new(),
        }),
    }
}

fn recycle(mut fr: Box<Frame>) {
    // Emptied first: what it held may give back frames of its own.
    fr.captured.clear();
    let pool = pool();
    if pool.len() < 64 {
        pool.push(fr);
    }
}

/// A descriptor's value, in `env`.
fn d(dd: D, env: &[V]) -> i64 {
    match dd {
        D::K(k) => k,
        D::S(s) => match env[s as usize] {
            V::Val(v) => v as i64,
            _ => desc::ANY,
        },
    }
}

/// Slot `s`'s value, for somewhere else: a word is copied, and a frame not
/// built -- used once, as the linear program uses everything it owns -- moved.
fn take_val(env: &mut [V], s: Slot) -> V {
    match &env[s as usize] {
        V::Val(v) => V::Val(*v),
        V::Frame(_) => std::mem::take(&mut env[s as usize]),
        V::Empty => fail("a name not in scope was read"),
    }
}

/// What giving `fr` up would give up: each value it owns, with its
/// descriptor, or `TOKEN` for a reuse token.
fn held(fr: &Frame, out: &mut SmallVec<[i64; 18]>) {
    for (c, dd, h) in &fr.captured {
        hold(c, *dd, *h, out);
    }
}

/// One of what a frame holds, as [`held`] records it.
fn hold(c: &V, dd: i64, h: Hold, out: &mut SmallVec<[i64; 18]>) {
    match (c, h) {
        (_, Hold::Borrowed) | (V::Empty, _) => {}
        (V::Frame(inner), _) => held(inner, out),
        (V::Val(v), Hold::Token) => {
            out.push(*v as i64);
            out.push(meadow_silo::shadow::TOKEN);
        }
        (V::Val(v), _) => {
            if dd == desc::REF || !is_known(dd) {
                out.push(*v as i64);
                out.push(dd);
            }
        }
    }
}

fn literal(k: K) -> Word {
    match k {
        K::W(w) => w,
        // Safety: bytes that live as long as the program.
        K::Text(t) => unsafe { meadow_silo::meadow_text(t.as_ptr(), t.len() as i64) },
        K::Big(n) => meadow_silo::meadow_bigint(n),
    }
}

/// A primitive the runtime does: `meadow_prim`, with the arguments and their
/// descriptors.
fn generic(p: Prim, vals: &[Word], descs: &[i64]) -> Word {
    // Safety: two arrays of as many words.
    unsafe {
        meadow_silo::prims::meadow_prim(
            i64::from(p.code()),
            vals.len() as i64,
            vals.as_ptr(),
            descs.as_ptr(),
        )
    }
}

/// A binary primitive that branches: inline, or the runtime's with both
/// operands described as the left one is.
fn binary(p: Prim, kind: PK, a: Word, b: Word, left: i64) -> Word {
    match kind {
        PK::Inline(q, float) => inline(q, float, a, b),
        _ => generic(p, &[a, b], &[left, left]),
    }
}

/// A closure whose first method is `base` in the table.
fn build_closure(base: u32, vals: &[Word], descs: &[i64]) -> Word {
    if vals.is_empty() {
        return (u64::from(base) << 1) | 1;
    }
    heap::build(heap::CLOSURE, base, vals, descs)
}

/// Where in the method table the closure `obj`'s methods begin: the number a
/// closure with no block carries, or its block's `meta`.
fn method_of(obj: Word) -> usize {
    if obj & 1 == 1 {
        (obj >> 1) as usize
    } else {
        heap::meta(obj) as usize
    }
}

/// `n` fields of the non-uniform block `v`.
fn load_fields(v: Word, n: usize) -> Words {
    if n == 0 {
        return Words::new();
    }
    let first = 2 + n.div_ceil(16);
    (0..n).map(|i| heap::word(v, first + i)).collect()
}

/// The references to `v` besides one.
fn count(v: Word) -> u32 {
    heap::word(v, 0) as u32
}

fn set_count(v: Word, rc: u32) {
    heap::set_word(v, 0, (heap::word(v, 0) & !0xFFFF_FFFF) | u64::from(rc));
}

/// Write the block at `b` whole: what `heap::build` writes into new memory.
fn write_block(b: Word, kind: u64, meta: u32, vals: &[Word], descs: &[i64]) {
    let n = vals.len();
    let dw = n.div_ceil(16);
    heap::set_word(b, 0, (n as u64) << 32);
    heap::set_word(b, 1, kind | (u64::from(meta) << 32));
    for w in 0..dw {
        let mut bits = 0u64;
        for (j, d) in descs.iter().enumerate().skip(16 * w).take(16) {
            bits |= ((*d as u64) & 15) << (4 * (j - 16 * w));
        }
        heap::set_word(b, 2 + w, bits);
    }
    for (i, x) in vals.iter().enumerate() {
        heap::set_word(b, 2 + dw + i, *x);
    }
}

/// Whether `d` is a descriptor known not to need counting -- anything but a
/// reference, or one of a type variable's, which says at run time.
fn is_known(d: i64) -> bool {
    (0..16).contains(&d)
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

fn fail(msg: &str) -> ! {
    // Safety: a message of this many bytes.
    unsafe { meadow_silo::meadow_fail(msg.as_ptr(), msg.len() as i64) }
}

/// A binary primitive on words, as the compiled code does it inline: `p`
/// without its type ([`untyped`]), on floats or on integers.
fn inline(p: Prim, float: bool, a: Word, b: Word) -> Word {
    let (x, y) = (a as i64, b as i64);
    let (fx, fy) = (f64::from_bits(a), f64::from_bits(b));
    let truth = |c: bool| Word::from(c);
    match p {
        Prim::Add => x.wrapping_add(y) as Word,
        Prim::Sub => x.wrapping_sub(y) as Word,
        Prim::Mul => x.wrapping_mul(y) as Word,
        Prim::BitAnd => a & b,
        Prim::BitOr => a | b,
        Prim::BitXor => a ^ b,
        Prim::Shl => (x << (b & 63)) as Word,
        Prim::Shr => (x >> (b & 63)) as Word,
        Prim::Div | Prim::Mod => {
            let div = p == Prim::Div;
            if y == 0 {
                fail(if div {
                    "division by zero"
                } else {
                    "modulo by zero"
                });
            }
            if y == -1 {
                return if div { x.wrapping_neg() as Word } else { 0 };
            }
            (if div { x / y } else { x % y }) as Word
        }
        Prim::AddF => (fx + fy).to_bits(),
        Prim::SubF => (fx - fy).to_bits(),
        Prim::MulF => (fx * fy).to_bits(),
        Prim::DivF => (fx / fy).to_bits(),
        q @ (Prim::Eq | Prim::Ne | Prim::Lt | Prim::Gt | Prim::Le | Prim::Ge) => {
            if float {
                truth(match q {
                    Prim::Eq => fx == fy,
                    Prim::Ne => fx != fy,
                    Prim::Lt => fx < fy,
                    Prim::Gt => fx > fy,
                    Prim::Le => fx <= fy,
                    _ => fx >= fy,
                })
            } else {
                truth(match q {
                    Prim::Eq => x == y,
                    Prim::Ne => x != y,
                    Prim::Lt => x < y,
                    Prim::Gt => x > y,
                    Prim::Le => x <= y,
                    _ => x >= y,
                })
            }
        }
        Prim::LtF => truth(fx < fy),
        Prim::GtF => truth(fx > fy),
        Prim::LeF => truth(fx <= fy),
        Prim::GeF => truth(fx >= fy),
        other => fail(&format!("{other:?} is not a primitive done inline")),
    }
}

/// Is `p`, whose left operand is represented as `rep`, done inline rather
/// than by the runtime? As `meadow_llvm::emit`'s `inline2`.
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
