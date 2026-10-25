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
//! array checked, and the return of an answer to the null continuation. A
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

use super::code::{Ins, PrimI};
use super::{B, Code, D, Env, LOCAL, Next, Op, Proc, V, Vm, Word};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

/// What the JIT was asked for.
pub(super) struct Jit {
    /// How many times a procedure is entered before it is compiled.
    threshold: u32,
    arena: Mutex<meadow_exec::Arena>,
}

impl Jit {
    /// The JIT `MEADOW_JIT` asks for, if this machine has one.
    pub(super) fn from_env() -> Result<Option<Jit>, String> {
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
        Ok(Some(Jit {
            threshold,
            arena: Mutex::new(meadow_exec::Arena::default()),
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
}

/// A procedure that was not compiled, and will not be.
const NEVER: *mut u8 = 1 as *mut u8;

impl Table {
    pub(super) fn new(procs: &[Proc], instructions: usize) -> Table {
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
}

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
            let Some(f) = self.native(*p, &|t| &t.code) else {
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

    /// Procedure `p`'s function, or its body (as `which` picks the table),
    /// compiling it if it has been entered often enough -- an entry it
    /// counts -- or nothing, to interpret it.
    #[inline]
    fn native(&self, p: u32, which: &dyn Fn(&Table) -> &[AtomicPtr<u8>]) -> Option<*mut u8> {
        let jit = self.jit.as_ref()?;
        let seg = self.seg(p);
        let local = (p & LOCAL) as usize;
        let mut f = which(&seg.jit)[local].load(Ordering::Acquire);
        if f.is_null() {
            let n = seg.jit.entered[local].fetch_add(1, Ordering::Relaxed);
            if n < jit.threshold {
                return None;
            }
            jit.compile(self, p);
            f = which(&seg.jit)[local].load(Ordering::Acquire);
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
        Next::Enter => match vm.native(cx.p, &|t| &t.body) {
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
        let (f, body) = translate(&seg.code, entry, end)
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
fn translate(_code: &Code, _entry: usize, _end: usize) -> Option<(Vec<u8>, usize)> {
    None
}

/// Where a slot's tag and word are, from the slots' address: a `V` is a tag
/// word and then its word.
#[cfg(target_arch = "x86_64")]
fn tag_of(s: u8) -> i32 {
    i32::from(s) * std::mem::size_of::<V>() as i32
}

#[cfg(target_arch = "x86_64")]
fn word_of(s: u8) -> i32 {
    tag_of(s) + 8
}

/// The tags a slot's `V` has: see the assertion in `super`.
#[cfg(target_arch = "x86_64")]
const VAL: i8 = 1;
#[cfg(target_arch = "x86_64")]
const FRAME: i8 = 2;

/// The machine code of the procedure whose instructions are `entry..end` of
/// `code`, and where in it the body begins: past the prologue, which every
/// function has the same, so that a tail call from one jumps to another's
/// body and leaves the same frame.
#[cfg(target_arch = "x86_64")]
fn translate(code: &Code, entry: usize, end: usize) -> Option<(Vec<u8>, usize)> {
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
    let call_step = |a: &mut Asm, pc: usize| {
        a.mov(ARG[0], RBX);
        a.mov_imm(ARG[1], pc as u64);
        a.mov_imm(RAX, step as *const () as usize as u64);
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
        let next = (pc + 1 < end).then(|| at(pc + 1));
        let slow = a.label();
        if !inline(&mut a, code, code.ins[pc], next, &at, slow) {
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
fn inline(
    a: &mut x64::Asm,
    code: &Code,
    ins: Ins,
    next: Option<x64::Label>,
    at: &dyn Fn(usize) -> x64::Label,
    slow: x64::Label,
) -> bool {
    use x64::*;
    // An operand, from a slot or a constant.
    #[derive(Clone, Copy)]
    enum Src {
        Slot(u8),
        K(u64),
    }
    // The slot holds a word, or it is the interpreter's to do.
    let want_word = |a: &mut Asm, s: u8| {
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
    let supported = |op: B| {
        cond(op).is_some()
            || matches!(
                op,
                B::Add | B::Sub | B::Mul | B::And | B::Or | B::Xor | B::Shl | B::Shr
            )
    };
    // `dst = a op src`.
    let bin = |a: &mut Asm, op: B, x: u8, src: Src, dst: u8| -> bool {
        let Some(next) = next else { return false };
        if !supported(op) {
            return false;
        }
        want_word(a, x);
        if let Src::Slot(s) = src {
            want_word(a, s);
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
    let branch = |a: &mut Asm, op: B, x: u8, src: Src, no: u32| -> bool {
        let Some(next) = next else { return false };
        if !supported(op) {
            return false;
        }
        want_word(a, x);
        if let Src::Slot(s) = src {
            want_word(a, s);
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
    let float = |a: &mut Asm, op: B, x: u8, src: Src, dst: u8| -> bool {
        let Some(next) = next else { return false };
        let sse = match op {
            B::FAdd => Sse::Add,
            B::FSub => Sse::Sub,
            B::FMul => Sse::Mul,
            B::FDiv => Sse::Div,
            _ => return false,
        };
        want_word(a, x);
        if let Src::Slot(s) = src {
            want_word(a, s);
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
        want_word(a, arr);
        want_word(a, idx);
        if let Some(v) = val {
            want_word(a, v);
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
    match ins {
        Ins::Prim(i) => element(a, &code.prims[i as usize]),
        Ins::Bin { op, a: x, b, dst } if is_float(op) => float(a, op, x, Src::Slot(b), dst),
        Ins::BinK { op, a: x, dst, k } if is_float(op) => {
            float(a, op, x, Src::K(code.consts[k as usize]), dst)
        }
        Ins::Bin { op, a: x, b, dst } => bin(a, op, x, Src::Slot(b), dst),
        Ins::BinI { op, a: x, dst, k } => bin(a, op, x, Src::K(k as i64 as u64), dst),
        Ins::BinK { op, a: x, dst, k } => bin(a, op, x, Src::K(code.consts[k as usize]), dst),
        Ins::IfBin { op, a: x, b, no } => branch(a, op, x, Src::Slot(b), no),
        Ins::IfBinI { op, a: x, k, no } => branch(a, op, x, Src::K(k as i64 as u64), no),
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
        assert_eq!((VAL, FRAME), (1, 2));
        let frame = V::Frame(Box::new(super::super::Frame {
            proc: 0,
            base: 0,
            captured: Default::default(),
        }));
        assert_eq!(words(&frame)[0], 2);
    }
}
