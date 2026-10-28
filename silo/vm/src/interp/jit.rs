//! **Tier 1 of the JIT**: a procedure the VM enters often, compiled to x86-64
//! over the VM's own registers (`docs/RUNTIME-MERGE.md`, "A JIT: the
//! design").
//!
//! A compiled procedure is a native function of a [`Cx`], which says where
//! the procedure's slots are -- its `Env`, as the interpreter keeps it -- and
//! what it runs in. Compiled and interpreted code see one state, so any
//! instruction can be left to the interpreter: one this tier does not
//! compile is a call to [`step`], which runs that instruction as the
//! interpreter's loop does ([`Vm::step`]) and answers where to go next.
//! Another pc of the procedure is a jump through a table of the function's
//! labels; anything else leaves the function, for the driver
//! ([`Vm::jitted`]) to enter the next procedure, compiled or interpreted.
//!
//! What is compiled inline is the integer arithmetic and comparisons and the
//! floating-point arithmetic, on slots holding words, with anything else -- a
//! slot holding a frame not built, a division -- left to [`step`] at the
//! same pc. So are reading and writing a mutable array's element of a type
//! that is no reference (`stGetArray`, `stSetArray`), with the bounds and the
//! array checked, and the return of an answer to the null continuation.
//! Under counting, so are sharing and erasing a reference: a count up or
//! down, and the cycle collector's candidate, as the emitted code's helpers
//! do it, with the last reference and a region's block the interpreter's. A
//! tail call (`Jump`, `Invoke`) is made by [`transfer`], and when
//! the procedure it enters is compiled too, the function jumps into that
//! one's body, past its prologue, with the same `Cx` and native frame: a loop
//! of tail calls stays in compiled code, and in one native frame.
//!
//! No value stays in a machine register across a call of [`step`], which may
//! collect, move blocks and resize the slots: the slots' address is read
//! again from the [`Cx`] after each, and every operand from the slots. So the
//! VM's records, which already reach the slots, are all the roots compiled
//! code has.
//!
//! `MEADOW_JIT` turns it on, and `MEADOW_JIT_THRESHOLD` (1000 by default)
//! says how many times a procedure is entered before it is compiled: 0
//! compiles each the first time. Only on x86-64, so far.

#[cfg(target_arch = "x86_64")]
mod x64;

use super::code::{Ins, PrimI, WideI};
use super::{Args, B, Callee, Code, D, Env, LOCAL, Next, Op, Proc, V, Vm, Word};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

/// What the JIT was asked for.
pub(super) struct Jit {
    /// How many times a procedure is entered before it is compiled.
    threshold: u32,
    arena: Mutex<meadow_exec::Arena>,
    /// The second tier, unless `MEADOW_JIT_TIERS=1`: see `super::tier2`.
    pub(super) tier2: Option<super::tier2::Tier2>,
}

impl Jit {
    /// The JIT `MEADOW_JIT` asks for, if this machine has one.
    pub(super) fn from_env(threaded: bool) -> Result<Option<Jit>, String> {
        let on = std::env::var("MEADOW_JIT")
            .ok()
            .is_some_and(|v| !matches!(v.as_str(), "" | "0" | "false" | "no"));
        if !on {
            return Ok(None);
        }
        if !cfg!(target_arch = "x86_64") {
            return Err("the JIT compiles for x86-64 only yet: run without MEADOW_JIT".into());
        }
        let threshold = match std::env::var("MEADOW_JIT_THRESHOLD") {
            Ok(n) => n
                .trim()
                .parse()
                .map_err(|_| format!("MEADOW_JIT_THRESHOLD is `{n}`, not a number of entries"))?,
            Err(_) => 1000,
        };
        let regs = meadow_llvm::CallConv::host().regs();
        Ok(Some(Jit {
            threshold,
            arena: Mutex::new(meadow_exec::Arena::default()),
            tier2: super::tier2::Tier2::from_env(threaded, regs)?,
        }))
    }
}

/// A segment's compiled procedures, by their number in it.
pub(super) struct Table {
    /// The native function, or null for none yet, or [`NEVER`].
    code: Box<[AtomicPtr<u8>]>,
    /// Where its body begins, past the prologue: what a tail call from
    /// another compiled procedure jumps to.
    body: Box<[AtomicPtr<u8>]>,
    /// How many times it has been entered, uncompiled.
    entered: Box<[AtomicU32]>,
    /// Where each procedure's instructions end.
    ends: Box<[u32]>,
    /// What the second tier has compiled of the segment.
    pub(super) tier2: super::tier2::SegTable,
}

/// A procedure that was not compiled, and will not be.
const NEVER: *mut u8 = 1 as *mut u8;

impl Table {
    pub(super) fn new(procs: &[Proc], instructions: usize, methods: usize) -> Table {
        let mut entries: Vec<u32> = procs.iter().map(|p| p.entry).collect();
        entries.sort_unstable();
        entries.dedup();
        let ends = procs
            .iter()
            .map(|p| {
                let next = entries.partition_point(|e| *e <= p.entry);
                entries.get(next).copied().unwrap_or(instructions as u32)
            })
            .collect();
        Table {
            code: procs.iter().map(|_| AtomicPtr::default()).collect(),
            body: procs.iter().map(|_| AtomicPtr::default()).collect(),
            entered: procs.iter().map(|_| AtomicU32::new(0)).collect(),
            ends,
            tier2: super::tier2::SegTable::new(procs.len(), methods),
        }
    }
}

/// What a compiled procedure runs with, and what it answers in. Its layout
/// is the compiled code's.
#[repr(C)]
pub(super) struct Cx {
    /// The slots: `env`'s, read again after every call of [`step`].
    slots: *mut V,
    vm: *const Vm,
    env: *mut Env,
    code: *const Code,
    /// What the procedure answers, or the pc it leaves the interpreter at.
    result: Word,
    /// The procedure: the one to enter next, when the function leaves for
    /// another.
    p: u32,
    /// Where a renaming's values wait between being read and being written,
    /// two words each: what a parallel move needs. Never read before it is
    /// written, so never cleared.
    scratch: std::mem::MaybeUninit<[u64; 2 * RENAMED_AT_MOST]>,
}

/// The most pairs a renaming compiled inline moves.
const RENAMED_AT_MOST: usize = 32;

/// How a compiled procedure ends: entering `cx.p`, answering `cx.result`, or
/// handing the interpreter pc `cx.result` of `cx.p`.
const ENTER: u64 = 0;
const DONE: u64 = 1;
const AT: u64 = 2;

type Native = unsafe extern "C" fn(*mut Cx) -> u64;

/// Where the driver goes on, after running compiled code.
pub(super) enum Jitted {
    /// The procedure answered.
    Done(Word),
    /// Interpret the procedure now in `p`, from its entry.
    Entry,
    /// Interpret it from this pc.
    At(usize),
}

impl Vm {
    /// Run procedure `*p`, with `env` its slots, natively for as long as each
    /// procedure it enters is compiled -- compiling one that has been
    /// entered often enough -- and say where the interpreter takes over.
    pub(super) fn jitted(&self, p: &mut u32, env: &mut Env) -> Jitted {
        loop {
            if meadow_silo::coverage::recording() {
                return Jitted::Entry;
            }
            // A build that migrates values: interpreted, where every
            // `switch` can migrate one (`Vm::migrated`).
            if self.interpreted[(*p >> super::SEG) as usize].load(Ordering::Relaxed) {
                return Jitted::Entry;
            }
            // A definition the second tier compiled: run as an executable's
            // code is, until it answers or leaves a tail call to the VM.
            if let Some((f, tramp)) = self.tier2(*p) {
                match self.run_tier2(f, tramp, *p, env) {
                    super::tier2::Ran::Done(w) => return Jitted::Done(w),
                    super::tier2::Ran::Pending(pend) => {
                        *p = self.enter_pending(pend, env);
                        continue;
                    }
                    super::tier2::Ran::Not => {}
                }
            }
            let Some(f) = self.native(*p, false) else {
                return Jitted::Entry;
            };
            let seg = self.seg(*p);
            let mut cx = Cx {
                slots: env.as_mut_ptr(),
                vm: self,
                env,
                code: &seg.code,
                result: 0,
                p: *p,
                scratch: std::mem::MaybeUninit::uninit(),
            };
            // Safety: a function compiled for this procedure, given the slots
            // it was compiled to read, as `Cx` lays them out.
            let how = unsafe { std::mem::transmute::<*mut u8, Native>(f)(&mut cx) };
            *p = cx.p;
            match how {
                ENTER => continue,
                DONE => return Jitted::Done(cx.result),
                _ => return Jitted::At(cx.result as usize),
            }
        }
    }

    /// Procedure `p` with `args`, to its native return, from compiled code:
    /// what [`Vm::call`] does, less what the caller has asked already --
    /// whether there is stack enough, whether records are kept.
    fn call_compiled(&self, p: u32, args: Args) -> Word {
        let mut env = Env::new();
        self.bind(p, args, &mut env);
        self.safe_point(p, &mut env);
        self.run::<false, true, false>(p, &mut env, None)
    }

    /// Procedure `p`'s function, or its body if `body`, compiling it if it
    /// has been entered often enough -- an entry it counts -- or nothing, to
    /// interpret it.
    #[inline]
    fn native(&self, p: u32, body: bool) -> Option<*mut u8> {
        let jit = self.jit.as_ref()?;
        let seg = self.seg(p);
        let local = (p & LOCAL) as usize;
        let which = if body { &seg.jit.body } else { &seg.jit.code };
        let mut f = which[local].load(Ordering::Acquire);
        if f.is_null() {
            // Counted without a locked instruction: two threads entering at
            // once may count one entry, which only puts compiling off.
            let counter = &seg.jit.entered[local];
            let n = counter.load(Ordering::Relaxed);
            counter.store(n + 1, Ordering::Relaxed);
            if n < jit.threshold {
                return None;
            }
            jit.compile(self, p);
            f = which[local].load(Ordering::Acquire);
        }
        (f != NEVER && !f.is_null()).then_some(f)
    }
}

/// Run the instruction at `pc` of the procedure `cx` is running, as the
/// interpreter would: the next pc of the procedure, or -1 when it enters
/// another (`cx.p`), or -2 when it answers (`cx.result`).
///
/// # Safety
///
/// Called by compiled code, with the `Cx` it was given.
unsafe extern "C" fn step(cx: *mut Cx, pc: u32) -> i64 {
    // Safety: the caller's: a `Cx` the driver made, whose pointers it holds.
    let cx = unsafe { &mut *cx };
    let (vm, env, code) = unsafe { (&*cx.vm, &mut *cx.env, &*cx.code) };
    if counting_steps() {
        count_step(code, pc as usize);
    }
    let next = match vm.step(&mut cx.p, env, code, pc as usize) {
        Next::At(n) => n as i64,
        Next::Enter => -1,
        Next::Done(w) => {
            cx.result = w;
            -2
        }
    };
    cx.slots = env.as_mut_ptr();
    next
}

/// Make the call by name at `pc` of the procedure `cx` is running, as the
/// interpreter would, and answer the next pc: the callee entered at once,
/// compiled if it is. A call whose frame holds anything on the shadow chain,
/// one with too little stack left, and any while records are kept, are
/// [`step`]'s.
///
/// # Safety
///
/// Called by compiled code, with the `Cx` it was given, at a `Call`.
unsafe extern "C" fn call(cx: *mut Cx, pc: u32) -> i64 {
    // Safety: as in `step`.
    let c = unsafe { &mut *cx };
    let (vm, env, code) = unsafe { (&*c.vm, &mut *c.env, &*c.code) };
    let Ins::Call(i) = code.ins[pc as usize] else {
        unreachable!("a call")
    };
    let call = &code.calls[i as usize];
    let Callee::Proc(q) = call.callee else {
        return unsafe { step(cx, pc) };
    };
    if (vm.shadow && !call.held.is_empty())
        || (vm.growable && meadow_silo::stack::low())
        || meadow_silo::coverage::recording()
    {
        return unsafe { step(cx, pc) };
    }
    let mut args: Args = call.args.iter().map(|a| V::Val(vm.val(*a, env))).collect();
    args.push(V::Val(0));
    let r = vm.call_compiled(vm.target(q), args);
    env[call.answer as usize] = V::Val(r);
    c.slots = env.as_mut_ptr();
    i64::from(pc) + 1
}

/// Whether `MEADOW_JIT_LOG=steps` asks how many of each instruction compiled
/// code left to [`step`]: what is worth compiling inline next.
fn counting_steps() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MEADOW_JIT_LOG").is_ok_and(|v| v == "steps"))
}

static STEPS: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

fn count_step(code: &Code, pc: usize) {
    let what = match code.ins[pc] {
        Ins::Prim(i) => match code.prims[i as usize].op {
            Op::Prim(p, _) | Op::PrimK(p, ..) => leak_name(format!("prim {p:?}")),
            ref op => leak_name(format!("prim {}", super::code::op_name(op))),
        },
        ref i => super::code::name(i),
    };
    let mut steps = STEPS.lock().unwrap_or_else(|e| e.into_inner());
    match steps.iter_mut().find(|(n, _)| *n == what) {
        Some((_, c)) => *c += 1,
        None => steps.push((what, 1)),
    }
}

/// A name, kept for the rest of the run: one per kind of instruction.
fn leak_name(s: String) -> &'static str {
    static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(n) = names.iter().find(|n| **n == s) {
        return n;
    }
    let n: &'static str = Box::leak(s.into_boxed_str());
    names.push(n);
    n
}

/// What [`counting_steps`] counted, most first: said when the program ends.
pub fn report_steps() {
    if !counting_steps() {
        return;
    }
    let mut steps = STEPS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    steps.sort_by(|a, b| b.1.cmp(&a.1));
    for (what, n) in steps {
        eprintln!("jit: {n:>12} {what}");
    }
}

/// Make the tail call at `pc` of the procedure `cx` is running, as the
/// interpreter would: the body of the procedure it enters, if that is
/// compiled -- with `cx` made that procedure's -- or [`ENTER`] for the driver
/// to enter it, [`DONE`] when it answered (`cx.result`), or [`AT`].
///
/// # Safety
///
/// Called by compiled code, with the `Cx` it was given.
unsafe extern "C" fn transfer(cx: *mut Cx, pc: u32) -> u64 {
    // Safety: as in `step`.
    let cx = unsafe { &mut *cx };
    let (vm, env, code) = unsafe { (&*cx.vm, &mut *cx.env, &*cx.code) };
    let how = vm.step(&mut cx.p, env, code, pc as usize);
    cx.slots = env.as_mut_ptr();
    match how {
        // Records begun are kept by the interpreter: the driver's to see.
        Next::Enter if meadow_silo::coverage::recording() => ENTER,
        Next::Enter => match vm.native(cx.p, true) {
            Some(body) => {
                cx.code = &vm.seg(cx.p).code;
                body as u64
            }
            None => ENTER,
        },
        Next::Done(w) => {
            cx.result = w;
            DONE
        }
        Next::At(n) => {
            cx.result = n as Word;
            AT
        }
    }
}

impl Jit {
    /// Compile procedure `p`, once however many threads get here: its
    /// function, or [`NEVER`].
    fn compile(&self, vm: &Vm, p: u32) -> *mut u8 {
        let seg = vm.seg(p);
        let local = (p & LOCAL) as usize;
        let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let had = seg.jit.code[local].load(Ordering::Acquire);
        if !had.is_null() {
            return had;
        }
        let entry = seg.procs[local].entry as usize;
        let end = seg.jit.ends[local] as usize;
        // Counting's instructions are inline where blocks are counted.
        let counted = !vm.tracing && meadow_silo::heap::LAYOUT.counted;
        let (f, body) = translate(&seg.code, entry, end, counted)
            .and_then(|(bytes, body)| {
                let f = arena.put(&bytes, 16)?;
                // Safety: within the function just put there.
                Some((f, unsafe { f.add(body) }))
            })
            .unwrap_or((NEVER, NEVER));
        if std::env::var_os("MEADOW_JIT_LOG").is_some() {
            eprintln!(
                "jit: {} ({} instructions){}",
                seg.procs[local].name,
                end - entry,
                if f == NEVER { ": not compiled" } else { "" }
            );
        }
        seg.jit.body[local].store(body, Ordering::Release);
        seg.jit.code[local].store(f, Ordering::Release);
        f
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn translate(_code: &Code, _entry: usize, _end: usize, _counted: bool) -> Option<(Vec<u8>, usize)> {
    None
}

/// The most of a procedure's instructions, in percent, that may be left to
/// `step` for it to be compiled: `MEADOW_JIT_STEPPED`, or 30.
#[cfg(target_arch = "x86_64")]
fn stepped_at_most() -> usize {
    static AT_MOST: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AT_MOST.get_or_init(|| {
        std::env::var("MEADOW_JIT_STEPPED")
            .ok()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(30)
    })
}

/// Whether `MEADOW_JIT_SKIP` -- names of instructions, as `--dis` counts
/// them, separated by commas -- leaves this one to `step`: for finding which
/// instruction's compiled form is at fault.
#[cfg(target_arch = "x86_64")]
fn skipped(ins: Ins) -> bool {
    static SKIP: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let skip = SKIP.get_or_init(|| {
        std::env::var("MEADOW_JIT_SKIP")
            .map(|s| s.split(',').map(|n| n.trim().to_string()).collect())
            .unwrap_or_default()
    });
    !skip.is_empty() && skip.iter().any(|n| n == super::code::name(&ins))
}

/// Where a slot's tag and word are, from the slots' address: a `V` is a tag
/// word and then its word.
#[cfg(target_arch = "x86_64")]
fn tag_of(s: impl Into<u32>) -> i32 {
    s.into() as i32 * std::mem::size_of::<V>() as i32
}

#[cfg(target_arch = "x86_64")]
fn word_of(s: impl Into<u32>) -> i32 {
    tag_of(s) + 8
}

/// What a block of `kind`, `meta` and fields described by `descs` begins
/// with, up to its first field: its header and its descriptors' words, as
/// `heap::build` writes them -- for a kind with no `MUTABLE` bit.
#[cfg(target_arch = "x86_64")]
fn block_prefix(kind: u64, meta: u32, descs: &[i64]) -> Vec<u64> {
    let l = meadow_silo::heap::LAYOUT;
    debug_assert!(kind == meadow_silo::heap::DATA || kind == meadow_silo::heap::CLOSURE);
    let n = descs.len();
    let mut words = vec![0u64; l.first_field(n, false)];
    let head = l.header(kind, n as u64, u64::from(meta));
    words[..l.header_words].copy_from_slice(&head[..l.header_words]);
    l.pack_descs(descs, |w, bits| {
        if w < l.header_words {
            words[w] |= bits;
        } else {
            words[w] = bits;
        }
    });
    words
}

/// The runtime's, for compiled code: in the C convention, so that it can call
/// them.
#[cfg(target_arch = "x86_64")]
extern "C" fn acquire(words: u64) -> u64 {
    meadow_silo::heap::acquire(words as usize)
}

#[cfg(target_arch = "x86_64")]
extern "C" fn clean(v: u64) {
    meadow_silo::heap::clean(v)
}

#[cfg(target_arch = "x86_64")]
extern "C" fn share(v: u64, d: i64) {
    meadow_silo::heap::share(v, d)
}

/// The tags a slot's `V` has: see the assertion in `super`.
#[cfg(target_arch = "x86_64")]
const EMPTY: i8 = 0;
#[cfg(target_arch = "x86_64")]
const VAL: i8 = 1;
#[cfg(target_arch = "x86_64")]
const FRAME: i8 = 2;

/// The machine code of the procedure whose instructions are `entry..end` of
/// `code`, and where in it the body begins: past the prologue, which every
/// function has the same, so that a tail call from one jumps to another's
/// body and leaves the same frame.
#[cfg(target_arch = "x86_64")]
fn translate(code: &Code, entry: usize, end: usize, counted: bool) -> Option<(Vec<u8>, usize)> {
    use std::mem::offset_of;
    use x64::*;
    let mut a = Asm::default();
    let labels: Vec<Label> = (entry..end).map(|_| a.label()).collect();
    let at = |pc: usize| labels[pc - entry];
    let (leave, dispatch, resume, table) = (a.label(), a.label(), a.label(), a.label());
    let slots = offset_of!(Cx, slots) as i32;
    let result = offset_of!(Cx, result) as i32;
    // rbx holds the `Cx`, r12 the slots; both are saved, and the stack is
    // left aligned, with room for a callee's spill area on Windows.
    a.push(RBX);
    a.push(R12);
    a.alu_imm(Alu::Sub, RSP, 40);
    a.mov(RBX, ARG[0]);
    let body = a.code.len();
    a.load(R12, RBX, slots);
    // Hand the instruction at `pc` to the interpreter, and go where it says.
    let call_helper = |a: &mut Asm, pc: usize, helper: u64| {
        a.mov(ARG[0], RBX);
        a.mov_imm(ARG[1], pc as u64);
        a.mov_imm(RAX, helper);
        a.call(RAX);
        a.load(R12, RBX, slots);
        a.test(RAX, RAX);
        a.jcc(Cc::S, leave);
        if pc + 1 < end {
            a.alu_imm(Alu::Cmp, RAX, (pc + 1) as i32);
            a.jcc(Cc::E, at(pc + 1));
        }
        a.jmp(dispatch);
    };
    let call_step = |a: &mut Asm, pc: usize| call_helper(a, pc, step as *const () as usize as u64);
    // Make the tail call at `pc`, and go into the next procedure's body if it
    // is compiled; leave for the driver if not.
    let out = a.label();
    let call_transfer = |a: &mut Asm, pc: usize| {
        a.mov(ARG[0], RBX);
        a.mov_imm(ARG[1], pc as u64);
        a.mov_imm(RAX, transfer as *const () as usize as u64);
        a.call(RAX);
        a.alu_imm(Alu::Cmp, RAX, AT as i32 + 1);
        a.jcc(Cc::B, out);
        a.jmp_reg(RAX);
    };
    // How many instructions are the interpreter's: each costs more through
    // `step` than in the interpreter's own loop, so a procedure mostly of
    // them is left interpreted.
    let mut stepped = 0;
    for pc in entry..end {
        a.bind(at(pc));
        if let Ins::Invoke(i) = code.ins[pc] {
            // The answer, to the null continuation: what returning is.
            let iv = &code.invokes[i as usize];
            if let (0, [arg]) = (iv.tag, iv.args.as_slice())
                && let (Ok(t), Ok(arg)) = (u8::try_from(iv.target), u8::try_from(*arg))
            {
                let not = a.label();
                a.cmp_mem_imm8(R12, tag_of(t), VAL);
                a.jcc(Cc::Ne, not);
                a.cmp_mem_imm8(R12, word_of(t), 0);
                a.jcc(Cc::Ne, not);
                a.cmp_mem_imm8(R12, tag_of(arg), VAL);
                a.jcc(Cc::Ne, not);
                a.load(RAX, R12, word_of(arg));
                a.store(RBX, result, RAX);
                a.mov_imm(RAX, DONE);
                a.jmp(out);
                a.bind(not);
            }
        }
        if matches!(code.ins[pc], Ins::Jump(_) | Ins::Invoke(_)) {
            call_transfer(&mut a, pc);
            continue;
        }
        if let Ins::Call(i) = code.ins[pc]
            && matches!(code.calls[i as usize].callee, Callee::Proc(_))
            && !skipped(code.ins[pc])
        {
            call_helper(&mut a, pc, call as *const () as usize as u64);
            continue;
        }
        let next = (pc + 1 < end).then(|| at(pc + 1));
        let slow = a.label();
        if skipped(code.ins[pc]) || !inline(&mut a, code, code.ins[pc], counted, next, &at, slow) {
            stepped += 1;
            call_step(&mut a, pc);
            continue;
        }
        a.bind(slow);
        call_step(&mut a, pc);
    }
    // -1 to enter another procedure, -2 to answer: what `not` makes of them
    // is what the driver is told.
    a.bind(leave);
    a.not(RAX);
    a.bind(out);
    a.alu_imm(Alu::Add, RSP, 40);
    a.pop(R12);
    a.pop(RBX);
    a.ret();
    // Another pc: one of this procedure's, through the table; any other, to
    // the interpreter.
    a.bind(dispatch);
    a.alu_imm(Alu::Sub, RAX, entry as i32);
    a.alu_imm(Alu::Cmp, RAX, (end - entry) as i32);
    a.jcc(Cc::Ae, resume);
    a.lea_label(R11, table);
    a.load_i32_indexed(RAX, R11, RAX);
    a.alu(Alu::Add, RAX, R11);
    a.jmp_reg(RAX);
    a.bind(resume);
    a.alu_imm(Alu::Add, RAX, entry as i32);
    a.store(RBX, result, RAX);
    a.mov_imm(RAX, AT);
    a.jmp(out);
    if stepped * 100 > (end - entry) * stepped_at_most() {
        return None;
    }
    a.align(4);
    a.bind(table);
    for pc in entry..end {
        a.offset(table, at(pc));
    }
    Some((a.finish(), body))
}

/// Write instruction `ins` inline, if this tier can: going on at `next` (the
/// next pc's label, if there is one) or wherever it branches to, and to
/// `slow` -- the interpreter at the same pc -- whenever the case is not one
/// it handles. Whether it wrote it.
#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
fn inline(
    a: &mut x64::Asm,
    code: &Code,
    ins: Ins,
    counted: bool,
    next: Option<x64::Label>,
    at: &dyn Fn(usize) -> x64::Label,
    slow: x64::Label,
) -> bool {
    use x64::*;
    // An operand, from a slot or a constant.
    #[derive(Clone, Copy)]
    enum Src {
        Slot(u32),
        K(u64),
    }
    // The slot holds a word, or it is the interpreter's to do.
    let want_word = |a: &mut Asm, s: u32| {
        a.cmp_mem_imm8(R12, tag_of(s), VAL);
        a.jcc(Cc::Ne, slow);
    };
    // `rax op= src`, for the operations that are one instruction.
    let apply = |a: &mut Asm, op: B, src: Src| -> bool {
        let alu = match op {
            B::Add => Some(Alu::Add),
            B::Sub => Some(Alu::Sub),
            B::And => Some(Alu::And),
            B::Or => Some(Alu::Or),
            B::Xor => Some(Alu::Xor),
            _ => None,
        };
        match (op, alu, src) {
            (_, Some(alu), Src::Slot(s)) => a.alu_mem(alu, RAX, R12, word_of(s)),
            (_, Some(alu), Src::K(k)) => match i32::try_from(k as i64) {
                Ok(k) => a.alu_imm(alu, RAX, k),
                Err(_) => {
                    a.mov_imm(RDX, k);
                    a.alu(alu, RAX, RDX);
                }
            },
            (B::Mul, _, Src::Slot(s)) => a.imul_mem(RAX, R12, word_of(s)),
            (B::Mul, _, Src::K(k)) => {
                a.mov_imm(RDX, k);
                a.imul(RAX, RDX);
            }
            (B::Shl | B::Shr, _, src) => {
                match src {
                    Src::Slot(s) => a.load(RCX, R12, word_of(s)),
                    Src::K(k) => a.mov_imm(RCX, k & 63),
                }
                a.shift_cl(RAX, op == B::Shr);
            }
            // As `B::run` divides: by nought is the interpreter's, which
            // fails; by -1 is a negation, or nought, where the machine's
            // division would trap on the least integer.
            (B::Div | B::Mod, _, src) => {
                let (minus, done) = (a.label(), a.label());
                match src {
                    Src::Slot(s) => {
                        a.load(RCX, R12, word_of(s));
                        a.test(RCX, RCX);
                        a.jcc(Cc::E, slow);
                    }
                    Src::K(k) => a.mov_imm(RCX, k),
                }
                a.alu_imm(Alu::Cmp, RCX, -1);
                a.jcc(Cc::E, minus);
                a.cqo();
                a.idiv(RCX);
                if op == B::Mod {
                    a.mov(RAX, RDX);
                }
                a.jmp(done);
                a.bind(minus);
                if op == B::Div {
                    a.neg(RAX);
                } else {
                    a.mov_imm(RAX, 0);
                }
                a.bind(done);
            }
            _ => return false,
        }
        true
    };
    // The comparison `op` makes, as a condition.
    let cond = |op: B| match op {
        B::Eq => Some(Cc::E),
        B::Ne => Some(Cc::Ne),
        B::Lt => Some(Cc::L),
        B::Gt => Some(Cc::G),
        B::Le => Some(Cc::Le),
        B::Ge => Some(Cc::Ge),
        _ => None,
    };
    let compare = |a: &mut Asm, src: Src| match src {
        Src::Slot(s) => a.alu_mem(Alu::Cmp, RAX, R12, word_of(s)),
        Src::K(k) => match i32::try_from(k as i64) {
            Ok(k) => a.alu_imm(Alu::Cmp, RAX, k),
            Err(_) => {
                a.mov_imm(RDX, k);
                a.alu(Alu::Cmp, RAX, RDX);
            }
        },
    };
    let supported = |op: B, src: Src| {
        cond(op).is_some()
            || matches!(
                op,
                B::Add | B::Sub | B::Mul | B::And | B::Or | B::Xor | B::Shl | B::Shr
            )
            || (matches!(op, B::Div | B::Mod) && !matches!(src, Src::K(0)))
    };
    // `dst = a op src`.
    let bin = |a: &mut Asm, op: B, x: u32, src: Src, dst: u32| -> bool {
        let Some(next) = next else { return false };
        if !supported(op, src) {
            return false;
        }
        want_word(a, x.into());
        if let Src::Slot(s) = src {
            want_word(a, s.into());
        }
        // A frame in the destination is the interpreter's to let go of.
        a.cmp_mem_imm8(R12, tag_of(dst), FRAME);
        a.jcc(Cc::E, slow);
        a.load(RAX, R12, word_of(x));
        match cond(op) {
            Some(cc) => {
                compare(a, src);
                a.set_rax(cc);
            }
            None => {
                apply(a, op, src);
            }
        }
        a.store_imm(R12, tag_of(dst), i32::from(VAL));
        a.store(R12, word_of(dst), RAX);
        a.jmp(next);
        true
    };
    // On at `next` if `a op src` holds, at `no` if not.
    let branch = |a: &mut Asm, op: B, x: u32, src: Src, no: u32| -> bool {
        let Some(next) = next else { return false };
        if !supported(op, src) {
            return false;
        }
        want_word(a, x.into());
        if let Src::Slot(s) = src {
            want_word(a, s.into());
        }
        a.load(RAX, R12, word_of(x));
        match cond(op) {
            Some(cc) => {
                compare(a, src);
                a.jcc(cc, next);
            }
            None => {
                apply(a, op, src);
                a.test(RAX, RAX);
                a.jcc(Cc::Ne, next);
            }
        }
        a.jmp(at(no as usize));
        true
    };
    // `dst = a op src`, on doubles.
    let float = |a: &mut Asm, op: B, x: u32, src: Src, dst: u32| -> bool {
        let Some(next) = next else { return false };
        let sse = match op {
            B::FAdd => Sse::Add,
            B::FSub => Sse::Sub,
            B::FMul => Sse::Mul,
            B::FDiv => Sse::Div,
            _ => return false,
        };
        want_word(a, x.into());
        if let Src::Slot(s) = src {
            want_word(a, s.into());
        }
        a.cmp_mem_imm8(R12, tag_of(dst), FRAME);
        a.jcc(Cc::E, slow);
        a.load_f64(0, R12, word_of(x));
        match src {
            Src::Slot(s) => a.sse_op_mem(sse, 0, R12, word_of(s)),
            Src::K(k) => {
                a.mov_imm(RAX, k);
                a.movq_to_xmm(1, RAX);
                a.sse_op(sse, 0, 1);
            }
        }
        a.store_imm(R12, tag_of(dst), i32::from(VAL));
        a.store_f64(R12, word_of(dst), 0);
        a.jmp(next);
        true
    };
    let is_float = |op: B| matches!(op, B::FAdd | B::FSub | B::FMul | B::FDiv);
    // A mutable array's element, read or written, of a type that is no
    // reference: nothing to count, erase or remember.
    let element = |a: &mut Asm, x: &PrimI| -> bool {
        use meadow_rt::desc;
        use meadow_silo::heap;
        let Some(next) = next else { return false };
        let Op::Prim(p, _) = x.op else { return false };
        let slot = |s: u32| u8::try_from(s).ok();
        let (arr, idx, val, result) = match (p, x.args.as_slice()) {
            (meadow_rt::Prim::StGetArray, [arr, idx]) => (arr.s, idx.s, None, x.result),
            (meadow_rt::Prim::StSetArray, [arr, idx, v]) => match v.d {
                D::K(k) if k != desc::REF && k != desc::ANY => (arr.s, idx.s, Some(v.s), x.result),
                _ => return false,
            },
            _ => return false,
        };
        let (Some(arr), Some(idx)) = (slot(arr), slot(idx)) else {
            return false;
        };
        let val = match val.map(slot) {
            Some(None) => return false,
            Some(Some(v)) => Some(v),
            None => None,
        };
        let result = match result.map(slot) {
            Some(None) => return false,
            Some(Some(r)) => Some(r),
            None => None,
        };
        if val.is_none() && result.is_none() {
            return false;
        }
        // A header field of the block at `rax`, into `rdx`.
        let header = |a: &mut Asm, f: meadow_rt::layout::Field| {
            a.load(RDX, RAX, 8 * f.word as i32);
            if f.shift > 0 {
                a.shr_imm(RDX, f.shift as u8);
            }
            if !f.tops() {
                a.alu_imm(Alu::And, RDX, f.mask() as i32);
            }
        };
        let l = heap::LAYOUT;
        want_word(a, arr.into());
        want_word(a, idx.into());
        if let Some(v) = val {
            want_word(a, v.into());
        }
        if let Some(r) = result {
            a.cmp_mem_imm8(R12, tag_of(r), FRAME);
            a.jcc(Cc::E, slow);
        }
        // A mutable array's block, whose elements are no references.
        a.load(RAX, R12, word_of(arr));
        a.test_imm(RAX, l.imm_mask as i32);
        a.jcc(Cc::Ne, slow);
        a.test(RAX, RAX);
        a.jcc(Cc::E, slow);
        header(a, l.kind);
        a.alu_imm(Alu::Cmp, RDX, heap::MUT_ARRAY as i32);
        a.jcc(Cc::Ne, slow);
        header(a, l.elem_desc);
        a.alu_imm(Alu::Cmp, RDX, desc::REF as i32);
        a.jcc(Cc::E, slow);
        a.alu_imm(Alu::Cmp, RDX, desc::ANY as i32);
        a.jcc(Cc::E, slow);
        // In bounds: an index below the length, as an unsigned number.
        a.load(RCX, R12, word_of(idx));
        header(a, l.len);
        a.alu(Alu::Cmp, RCX, RDX);
        a.jcc(Cc::Ae, slow);
        let elems = 8 * heap::ELEMS as i32;
        match val {
            None => {
                a.load_indexed(RAX, RAX, RCX, elems);
            }
            Some(v) => {
                a.load(RDX, R12, word_of(v));
                a.store_indexed(RAX, RCX, elems, RDX);
                a.mov_imm(RAX, 0);
            }
        }
        if let Some(r) = result {
            a.store_imm(R12, tag_of(r), i32::from(VAL));
            a.store(R12, word_of(r), RAX);
        }
        a.jmp(next);
        true
    };
    // Share the value in slot `s`, described by `d`, or -- if `erase` --
    // erase it and empty the slot, as the interpreter does.
    let counting = |a: &mut Asm, s: u32, d: D, erase: bool| -> bool {
        use meadow_rt::desc;
        use meadow_silo::heap;
        let Some(next) = next else { return false };
        if !counted {
            return false;
        }
        let refs = match d {
            D::K(k) => k == desc::REF,
            D::S(_) => return false,
        };
        want_word(a, s.into());
        let emptied = |a: &mut Asm| {
            if erase {
                a.store_imm(R12, tag_of(s), 0);
            }
            a.jmp(next);
        };
        if !refs {
            emptied(a);
            return true;
        }
        let l = heap::LAYOUT;
        let nothing = a.label();
        a.load(RAX, R12, word_of(s));
        a.test_imm(RAX, l.imm_mask as i32);
        a.jcc(Cc::Ne, nothing);
        a.test(RAX, RAX);
        a.jcc(Cc::E, nothing);
        if erase {
            // The last reference, or one into a region: the interpreter's.
            a.load32(RDX, RAX);
            a.test32(RDX, RDX);
            a.jcc(Cc::E, slow);
            a.alu_imm(Alu::Cmp, RDX, heap::REGION_FLOOR as i32);
            a.jcc(Cc::A, slow);
            a.step32(RAX, true);
            a.store_imm(R12, tag_of(s), 0);
            // A candidate for the cycle collector: a block of a kind a cycle
            // can run through, not yet coloured -- the emitted code's test.
            let mask = l.mutable.bits_in_place() | l.colour.bits_in_place();
            a.load(RDX, RAX, 8 * l.flags_word() as i32);
            a.alu_imm(Alu::And, RDX, mask as i32);
            a.alu_imm(Alu::Cmp, RDX, l.mutable.bits_in_place() as i32);
            a.jcc(Cc::Ne, next);
            a.mov(ARG[0], RAX);
            a.mov_imm(
                RAX,
                meadow_silo::cycles::meadow_candidate as *const () as usize as u64,
            );
            a.call(RAX);
            a.load(R12, RBX, std::mem::offset_of!(Cx, slots) as i32);
            a.jmp(next);
        } else {
            // A share into a region tells the region: the interpreter's.
            a.cmp32_imm(RAX, heap::REGION_FLOOR);
            a.jcc(Cc::Ae, slow);
            a.step32(RAX, false);
            a.jmp(next);
        }
        a.bind(nothing);
        emptied(a);
        true
    };
    // Moves, all read before any is written, as the interpreter takes them: a
    // word is copied and a frame not built moved, its slot left empty. A
    // destination holding a frame the move does not take is the
    // interpreter's, which lets that frame go.
    let rename = |a: &mut Asm, pairs: &[(u32, u32)]| -> bool {
        let Some(next) = next else { return false };
        if pairs.len() > RENAMED_AT_MOST {
            return false;
        }
        let scratch = std::mem::offset_of!(Cx, scratch) as i32;
        for &(_, from) in pairs {
            a.cmp_mem_imm8(R12, tag_of(from), EMPTY);
            a.jcc(Cc::E, slow);
        }
        for &(to, _) in pairs {
            if !pairs.iter().any(|&(_, from)| from == to) {
                a.cmp_mem_imm8(R12, tag_of(to), FRAME);
                a.jcc(Cc::E, slow);
            }
        }
        for (k, &(_, from)) in pairs.iter().enumerate() {
            let kept = a.label();
            a.load(RDX, R12, tag_of(from));
            a.store(RBX, scratch + 16 * k as i32, RDX);
            a.load(RCX, R12, word_of(from));
            a.store(RBX, scratch + 16 * k as i32 + 8, RCX);
            a.alu_imm(Alu::Cmp, RDX, i32::from(FRAME));
            a.jcc(Cc::Ne, kept);
            a.store_imm(R12, tag_of(from), i32::from(EMPTY));
            a.bind(kept);
        }
        for (k, &(to, _)) in pairs.iter().enumerate() {
            a.load(RDX, RBX, scratch + 16 * k as i32);
            a.store(R12, tag_of(to), RDX);
            a.load(RCX, RBX, scratch + 16 * k as i32 + 8);
            a.store(R12, word_of(to), RCX);
        }
        a.jmp(next);
        true
    };
    // A literal that is a word.
    let literal = |a: &mut Asm, x: &PrimI| -> bool {
        let Some(next) = next else { return false };
        let (Op::Lit(super::K::W(w)), Some(r)) = (&x.op, x.result) else {
            return false;
        };
        a.cmp_mem_imm8(R12, tag_of(r), FRAME);
        a.jcc(Cc::E, slow);
        a.store_imm(R12, tag_of(r), i32::from(VAL));
        a.mov_imm(RDX, *w);
        a.store(R12, word_of(r), RDX);
        a.jmp(next);
        true
    };
    // Share the reference in rax, and go on at `done`: a count up, or the
    // runtime's for a block in a region.
    let share_rax = |a: &mut Asm, done: Label| {
        use meadow_silo::heap;
        let l = heap::LAYOUT;
        let region = a.label();
        a.test_imm(RAX, l.imm_mask as i32);
        a.jcc(Cc::Ne, done);
        a.test(RAX, RAX);
        a.jcc(Cc::E, done);
        a.cmp32_imm(RAX, heap::REGION_FLOOR);
        a.jcc(Cc::Ae, region);
        a.step32(RAX, false);
        a.jmp(done);
        a.bind(region);
        a.mov(ARG[0], RAX);
        a.mov_imm(ARG[1], meadow_rt::desc::REF as u64);
        a.mov_imm(RAX, share as *const () as usize as u64);
        a.call(RAX);
        a.load(R12, RBX, std::mem::offset_of!(Cx, slots) as i32);
        a.jmp(done);
    };
    // Share the word in slot `s`, described by `d`, as `Vm::share` does: only
    // a reference is counted.
    let share_slot = |a: &mut Asm, s: u32, d: D| {
        use meadow_rt::desc;
        let done = a.label();
        match d {
            D::K(k) if k != desc::REF => return,
            D::K(_) => {}
            D::S(ds) => {
                // A descriptor not held as a word is `ANY`, which is not
                // counted.
                a.cmp_mem_imm8(R12, tag_of(ds), VAL);
                a.jcc(Cc::Ne, done);
                a.cmp_mem_imm8(R12, word_of(ds), desc::REF as i8);
                a.jcc(Cc::Ne, done);
            }
        }
        a.load(RAX, R12, word_of(s));
        share_rax(a, done);
        a.bind(done);
    };
    // A block's header field, of the block at `rax`, into `rdx`.
    let header = |a: &mut Asm, f: meadow_rt::layout::Field| {
        a.load(RDX, RAX, 8 * f.word as i32);
        if f.shift > 0 {
            a.shr_imm(RDX, f.shift as u8);
        }
        if !f.tops() {
            a.alu_imm(Alu::And, RDX, f.mask() as i32);
        }
    };
    // A switch on the tag of a value: its arm's fields loaded into their
    // slots and, unless the arm keeps the value, the value released -- the
    // last reference's block cleaned or kept for reuse, any other's count
    // down and its fields shared. A block in a region, a scrutinee that is a
    // frame not built, and a destination holding one are the interpreter's:
    // nothing is changed before those are ruled out but slots written with
    // what the interpreter would write in them again.
    let switch = |a: &mut Asm, sw: &super::code::SwitchI| -> bool {
        use meadow_silo::heap;
        let l = heap::LAYOUT;
        if l.imm_mask > i32::MAX as u64 || (!l.meta.tops() && l.meta.mask() > i32::MAX as u64) {
            return false;
        }
        // A token kept from a value of no fields: the interpreter's.
        if sw
            .arms
            .iter()
            .any(|arm| arm.fields.is_empty() && arm.reuse.is_some())
        {
            return false;
        }
        let s = sw.scrutinee;
        a.cmp_mem_imm8(R12, tag_of(s), VAL);
        a.jcc(Cc::Ne, slow);
        a.load(RAX, R12, word_of(s));
        // The tag: an immediate's payload, or a block's `meta`.
        let (block, have) = (a.label(), a.label());
        a.test_imm(RAX, l.imm_mask as i32);
        a.jcc(Cc::E, block);
        a.mov(RDX, RAX);
        a.shr_imm(RDX, l.imm_shift as u8);
        a.mov32(RDX, RDX);
        a.jmp(have);
        a.bind(block);
        header(a, l.meta);
        a.bind(have);
        let arms: Vec<Label> = sw.arms.iter().map(|_| a.label()).collect();
        for (arm, lbl) in sw.arms.iter().zip(&arms) {
            match i32::try_from(arm.tag) {
                Ok(t) => a.alu_imm(Alu::Cmp, RDX, t),
                Err(_) => {
                    a.mov_imm(RCX, arm.tag);
                    a.alu(Alu::Cmp, RDX, RCX);
                }
            }
            a.jcc(Cc::E, *lbl);
        }
        // Erase the value, a reference, and go on at `to`: its count down, or
        // the interpreter's for the last reference or a block in a region --
        // before anything is changed.
        let erase_rax = |a: &mut Asm, to: Label| {
            let go = a.label();
            a.test_imm(RAX, l.imm_mask as i32);
            a.jcc(Cc::Ne, go);
            a.test(RAX, RAX);
            a.jcc(Cc::E, go);
            a.load32(RDX, RAX);
            a.test32(RDX, RDX);
            a.jcc(Cc::E, slow);
            a.alu_imm(Alu::Cmp, RDX, heap::REGION_FLOOR as i32);
            a.jcc(Cc::A, slow);
            a.step32(RAX, true);
            let mask = l.mutable.bits_in_place() | l.colour.bits_in_place();
            a.load(RDX, RAX, 8 * l.flags_word() as i32);
            a.alu_imm(Alu::And, RDX, mask as i32);
            a.alu_imm(Alu::Cmp, RDX, l.mutable.bits_in_place() as i32);
            a.jcc(Cc::Ne, go);
            a.mov(ARG[0], RAX);
            a.mov_imm(
                RAX,
                meadow_silo::cycles::meadow_candidate as *const () as usize as u64,
            );
            a.call(RAX);
            a.load(R12, RBX, std::mem::offset_of!(Cx, slots) as i32);
            a.bind(go);
            a.jmp(to);
        };
        // No arm's: the value erased, unless the default keeps it.
        if !sw.keep_default && counted {
            erase_rax(a, at(sw.default as usize));
        } else {
            a.jmp(at(sw.default as usize));
        }
        for (arm, lbl) in sw.arms.iter().zip(&arms) {
            a.bind(*lbl);
            let n = arm.fields.len();
            // An arm of no fields releases its value by erasing it, as
            // `Vm::release` does: most often it is no block at all.
            if n == 0 && !arm.keep && counted && arm.reuse.is_none() {
                erase_rax(a, at(arm.pc as usize));
                continue;
            }
            for u in &arm.fields {
                a.cmp_mem_imm8(R12, tag_of(u.s), FRAME);
                a.jcc(Cc::E, slow);
            }
            if let Some(t) = arm.reuse {
                a.cmp_mem_imm8(R12, tag_of(t), FRAME);
                a.jcc(Cc::E, slow);
            }
            let first = l.first_field(n, false) as i32;
            for (i, u) in arm.fields.iter().enumerate() {
                a.load(RDX, RAX, 8 * (first + i as i32));
                a.store_imm(R12, tag_of(u.s), i32::from(VAL));
                a.store(R12, word_of(u.s), RDX);
            }
            if arm.keep || !counted {
                if let (Some(t), false) = (arm.reuse, arm.keep) {
                    // Traced, nothing is kept for reuse.
                    a.store_imm(R12, tag_of(t), i32::from(VAL));
                    a.store_imm(R12, word_of(t), 0);
                }
                a.jmp(at(arm.pc as usize));
                continue;
            }
            let shared = a.label();
            a.load32(RCX, RAX);
            a.test32(RCX, RCX);
            a.jcc(Cc::Ne, shared);
            // The last reference: the block is the arm's to reuse, or clean.
            match arm.reuse {
                Some(t) => {
                    a.store_imm(R12, tag_of(t), i32::from(VAL));
                    a.store(R12, word_of(t), RAX);
                }
                None => {
                    a.mov(ARG[0], RAX);
                    a.mov_imm(RAX, clean as *const () as usize as u64);
                    a.call(RAX);
                    a.load(R12, RBX, std::mem::offset_of!(Cx, slots) as i32);
                }
            }
            a.jmp(at(arm.pc as usize));
            a.bind(shared);
            a.alu_imm(Alu::Cmp, RCX, heap::REGION_FLOOR as i32);
            a.jcc(Cc::A, slow);
            a.step32(RAX, true);
            for u in &arm.fields {
                share_slot(a, u.s, u.d);
            }
            if let Some(t) = arm.reuse {
                a.store_imm(R12, tag_of(t), i32::from(VAL));
                a.store_imm(R12, word_of(t), 0);
            }
            a.jmp(at(arm.pc as usize));
        }
        true
    };
    // A block of `kind` and `meta` holding the values in `fields`, into
    // `dst`: built in the reuse token's block when there is one, which is
    // this size, or in a new one. What describes the fields must be known
    // here, for the header to be.
    let build = |a: &mut Asm,
                 kind: u64,
                 meta: u32,
                 fields: &[super::U],
                 reuse: Option<u32>,
                 dst: u32|
     -> bool {
        use meadow_silo::heap;
        let Some(next) = next else { return false };
        let mut descs = Vec::with_capacity(fields.len());
        for u in fields {
            match u.d {
                D::K(k) => descs.push(k),
                D::S(_) => return false,
            }
        }
        for u in fields {
            want_word(a, u.s);
        }
        if let Some(t) = reuse {
            want_word(a, t);
        }
        a.cmp_mem_imm8(R12, tag_of(dst), FRAME);
        a.jcc(Cc::E, slow);
        if fields.is_empty() {
            if let Some(t) = reuse {
                a.store_imm(R12, tag_of(t), i32::from(EMPTY));
            }
            a.store_imm(R12, tag_of(dst), i32::from(VAL));
            a.mov_imm(RDX, heap::immediate(u64::from(meta)));
            a.store(R12, word_of(dst), RDX);
            a.jmp(next);
            return true;
        }
        let prefix = block_prefix(kind, meta, &descs);
        let write = a.label();
        if let Some(t) = reuse {
            a.load(RAX, R12, word_of(t));
            a.store_imm(R12, tag_of(t), i32::from(EMPTY));
            a.test(RAX, RAX);
            a.jcc(Cc::Ne, write);
        }
        a.mov_imm(ARG[0], (prefix.len() + fields.len()) as u64);
        a.mov_imm(RAX, acquire as *const () as usize as u64);
        a.call(RAX);
        a.load(R12, RBX, std::mem::offset_of!(Cx, slots) as i32);
        a.bind(write);
        for (w, k) in prefix.iter().enumerate() {
            match i32::try_from(*k as i64) {
                Ok(k) => a.store_imm(RAX, 8 * w as i32, k),
                Err(_) => {
                    a.mov_imm(RDX, *k);
                    a.store(RAX, 8 * w as i32, RDX);
                }
            }
        }
        for (i, u) in fields.iter().enumerate() {
            a.load(RDX, R12, word_of(u.s));
            a.store(RAX, 8 * (prefix.len() + i) as i32, RDX);
        }
        a.store_imm(R12, tag_of(dst), i32::from(VAL));
        a.store(R12, word_of(dst), RAX);
        a.jmp(next);
        true
    };
    // A reuse token given up: its block cleaned, if it holds one and blocks
    // are counted, and its slot emptied.
    let clean_token = |a: &mut Asm, t: u32| -> bool {
        let Some(next) = next else { return false };
        want_word(a, t);
        a.load(RAX, R12, word_of(t));
        a.store_imm(R12, tag_of(t), i32::from(EMPTY));
        if counted {
            a.test(RAX, RAX);
            a.jcc(Cc::E, next);
            a.mov(ARG[0], RAX);
            a.mov_imm(RAX, clean as *const () as usize as u64);
            a.call(RAX);
            a.load(R12, RBX, std::mem::offset_of!(Cx, slots) as i32);
        }
        a.jmp(next);
        true
    };
    match ins {
        Ins::Rename(i) => rename(a, &code.renames[i as usize]),
        Ins::Switch(i) => switch(a, &code.switches[i as usize]),
        Ins::Let(i) => {
            let l = &code.lets[i as usize];
            build(a, meadow_silo::heap::DATA, l.tag, &l.fields, l.reuse, l.dst)
        }
        Ins::New(i) => {
            let x = &code.news[i as usize];
            x.frame.is_none()
                && build(
                    a,
                    meadow_silo::heap::CLOSURE,
                    x.base,
                    &x.captures,
                    None,
                    x.dst,
                )
        }
        Ins::Clean(t) => clean_token(a, t),
        Ins::Prim(i) if matches!(code.prims[i as usize].op, Op::Lit(_)) => {
            literal(a, &code.prims[i as usize])
        }
        Ins::Share { s, d } => counting(a, s.into(), d.get(), false),
        Ins::Erase { s, d } => counting(a, s.into(), d.get(), true),
        Ins::Wide(w) => match code.wides[w as usize] {
            WideI::Share { s, d } => counting(a, s, d, false),
            WideI::Erase { s, d } => counting(a, s, d, true),
            WideI::Bin { op, a: x, b, dst } if is_float(op) => float(a, op, x, Src::Slot(b), dst),
            WideI::Bin { op, a: x, b, dst } => bin(a, op, x, Src::Slot(b), dst),
            WideI::BinK { op, a: x, k, dst } if is_float(op) => float(a, op, x, Src::K(k), dst),
            WideI::BinK { op, a: x, k, dst } => bin(a, op, x, Src::K(k), dst),
            WideI::IfBin { op, a: x, b, no } => branch(a, op, x, Src::Slot(b), no),
            WideI::IfBinK { op, a: x, k, no } => branch(a, op, x, Src::K(k), no),
        },
        Ins::Prim(i) => element(a, &code.prims[i as usize]),
        Ins::Bin { op, a: x, b, dst } if is_float(op) => {
            float(a, op, x.into(), Src::Slot(b.into()), dst.into())
        }
        Ins::BinK { op, a: x, dst, k } if is_float(op) => {
            float(a, op, x.into(), Src::K(code.consts[k as usize]), dst.into())
        }
        Ins::Bin { op, a: x, b, dst } => bin(a, op, x.into(), Src::Slot(b.into()), dst.into()),
        Ins::BinI { op, a: x, dst, k } => bin(a, op, x.into(), Src::K(k as i64 as u64), dst.into()),
        Ins::BinK { op, a: x, dst, k } => {
            bin(a, op, x.into(), Src::K(code.consts[k as usize]), dst.into())
        }
        Ins::IfBin { op, a: x, b, no } => branch(a, op, x.into(), Src::Slot(b.into()), no),
        Ins::IfBinI { op, a: x, k, no } => branch(a, op, x.into(), Src::K(k as i64 as u64), no),
        Ins::Branch(t) => {
            let t = &code.tests[t as usize];
            match (&t.test, t.args.as_slice()) {
                // A word that is not nought.
                (super::Test::Truth, [u]) => branch(a, B::Ne, u.s, Src::K(0), t.no),
                (super::Test::Prim(_, super::PK::Inline(q, float)), [x, y]) => {
                    match B::of(*q, *float) {
                        Some(op) => branch(a, op, x.s, Src::Slot(y.s), t.no),
                        None => false,
                    }
                }
                _ => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What compiled code reads of a slot: the tag word, and the word.
    #[test]
    fn a_slot_is_a_tag_and_then_its_word() {
        let words = |v: &V| unsafe { *(v as *const V as *const [u64; 2]) };
        assert_eq!(words(&V::Empty)[0], 0);
        assert_eq!(words(&V::Val(42)), [1, 42]);
        #[cfg(target_arch = "x86_64")]
        assert_eq!((EMPTY, VAL, FRAME), (0, 1, 2));
        let frame = V::Frame(Box::new(super::super::Frame {
            proc: 0,
            base: 0,
            captured: Default::default(),
        }));
        assert_eq!(words(&frame)[0], 2);
    }
}
