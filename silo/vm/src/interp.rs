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
mod jit;
pub use jit::report_steps;
mod live;
pub mod profile;
mod tier2;
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

/// Where the interpreter goes after an instruction: to another pc of the
/// procedure it is in, to the entry of the procedure it has made current
/// with its slots, or back to whoever called it, with an answer.
enum Next {
    At(usize),
    Enter,
    Done(Word),
}

/// The instruction at `$pc` of `$code`, run in procedure `$p` with `$env`:
/// what the interpreter does at each step, written once for the two places
/// that do it -- the interpreter's loop, which keeps its state in locals, and
/// [`Vm::step`], which compiled code asks to run an instruction it does not
/// compile (`crate::jit`). Each says what going on means: `$go!(pc)` to
/// another pc of the procedure, `$enter!()` to the entry of the procedure
/// `$p` now is, with `$env` its slots, `$done!(word)` back to the caller,
/// and `$set_env!(env)` to replace the slots. `$jit` says whether the calls
/// it makes run with the JIT: so that the interpreter without it calls only
/// code without it, and pays nothing for its being there.
macro_rules! instruction {
    ($vm:ident, $jit:expr, $p:ident, $env:ident, $code:ident, $pc:ident, $go:ident, $enter:ident, $done:ident, $set_env:ident, $wait_frame:ident, $wait_held:ident) => {
        // Safety: every pc is a procedure's entry, an instruction's
        // successor within its procedure, or a target `Code::emit` patched
        // in -- all inside the array.
        match unsafe { *$code.ins.get_unchecked($pc) } {
            Ins::Share { s, d: dd } => {
                let v = $vm.val(s.into(), &mut $env);
                $vm.share(v, d(dd.get(), &$env));
                $go!($pc + 1);
            }
            Ins::Erase { s, d: dd } => {
                $vm.erase_slot(s.into(), dd.get(), &mut $env);
                $go!($pc + 1);
            }
            Ins::Rename(i) => {
                let moved: SmallVec<[(Slot, V); 4]> = $code.renames[i as usize]
                    .iter()
                    .map(|(to, from)| (*to, take_val(&mut $env, *from)))
                    .collect();
                for (to, v) in moved {
                    $env[to as usize] = v;
                }
                $go!($pc + 1);
            }
            Ins::Let(i) => {
                let l = &$code.lets[i as usize];
                let vals: Words = l.fields.iter().map(|u| $vm.val(u.s, &mut $env)).collect();
                let descs: Descs = l.fields.iter().map(|u| d(u.d, &$env)).collect();
                let token = l.reuse.map(|t| {
                    let v = $vm.val(t, &mut $env);
                    $env[t as usize] = V::Empty;
                    v
                });
                let v = $vm.build_in(l.tag, &vals, &descs, token);
                $env[l.dst as usize] = V::Val(v);
                $go!($pc + 1);
            }
            Ins::Clean(t) => {
                let tok = $vm.val(t, &mut $env);
                $vm.clean_token(tok);
                $env[t as usize] = V::Empty;
                $go!($pc + 1);
            }
            Ins::DropReuse(i) => {
                let x = &$code.drops[i as usize];
                let v = $vm.val(x.src, &mut $env);
                let tok = $vm.drop_reuse(v, &x.fields, &$env);
                $env[x.src as usize] = V::Empty;
                $env[x.token as usize] = V::Val(tok);
                $go!($pc + 1);
            }
            Ins::Switch(i) => {
                let sw = &$code.switches[i as usize];
                let v = $vm.val(sw.scrutinee, &mut $env);
                let tag = u64::from(heap::meta_of(v));
                match sw.arms.iter().find(|a| a.tag == tag) {
                    Some(a) => {
                        let loaded = load_fields(v, a.fields.len());
                        for (u, x) in a.fields.iter().zip(&loaded) {
                            $env[u.s as usize] = V::Val(*x);
                        }
                        if !a.keep {
                            let descs: Descs = a.fields.iter().map(|u| d(u.d, &$env)).collect();
                            match a.reuse {
                                Some(t) => {
                                    let tok = $vm.release_reuse(v, &loaded, &descs);
                                    $env[t as usize] = V::Val(tok);
                                }
                                None => $vm.release(v, &loaded, &descs),
                            }
                        }
                        $go!(a.pc as usize);
                    }
                    None => {
                        if !sw.keep_default {
                            $vm.erase(v, desc::REF);
                        }
                        $go!(sw.default as usize);
                    }
                }
            }
            Ins::New(i) => {
                let x = &$code.news[i as usize];
                let v = match &x.frame {
                    Some((fp, holds)) => {
                        let mut fr = new_frame(*fp, x.base);
                        for (u, h) in x.captures.iter().zip(holds) {
                            let dd = d(u.d, &$env);
                            fr.captured.push((take_val(&mut $env, u.s), dd, *h));
                        }
                        V::Frame(fr)
                    }
                    None => {
                        let vals: Words =
                            x.captures.iter().map(|u| $vm.val(u.s, &mut $env)).collect();
                        let descs: Descs = x.captures.iter().map(|u| d(u.d, &$env)).collect();
                        V::Val(build_closure(x.base, &vals, &descs))
                    }
                };
                $env[x.dst as usize] = v;
                $go!($pc + 1);
            }
            Ins::Jump(i) => {
                let j = &$code.jumps[i as usize];
                let args = &j.args;
                if !args
                    .iter()
                    .any(|a| matches!($env[*a as usize], V::Frame(_)))
                {
                    // No frame among them: the words, into the frame of
                    // words this procedure's slots reuse. What is left in
                    // the others is never read before it is written.
                    let words: Words = args.iter().map(|a| $vm.word(*a, &mut $env)).collect();
                    $p = $vm.target(j.proc);
                    let need = $vm.proc($p).slots.max(words.len());
                    if $env.len() < need {
                        $env.resize_with(need, V::default);
                    }
                    for (slot, w) in $env.iter_mut().zip(words) {
                        *slot = V::Val(w);
                    }
                    $vm.safe_point($p, &mut $env);
                    $enter!();
                }
                let (vals, frame) = $vm.transfer(args, &mut $env);
                match frame {
                    None => {
                        $p = $vm.target(j.proc);
                        $vm.bind($p, vals, &mut $env);
                        $vm.safe_point($p, &mut $env);
                    }
                    Some(fr) => $wait_frame!(fr, Target::Def($vm.target(j.proc)), vals),
                }
                $enter!();
            }
            Ins::Invoke(i) => {
                let iv = &$code.invokes[i as usize];
                let (target, tag, args) = (iv.target, iv.tag, &iv.args);
                if let V::Frame(_) = &$env[target as usize] {
                    // Our own frame, entered directly: its $code, here.
                    let V::Frame(fr) = std::mem::take(&mut $env[target as usize]) else {
                        unreachable!()
                    };
                    let [a] = args.as_slice() else {
                        fail("a frame takes one answer")
                    };
                    let a = take_val(&mut $env, *a);
                    $p = $vm.enter_frame(fr, a, &mut $env);
                    $enter!();
                }
                let t = $vm.val(target, &mut $env);
                // A continuation of one argument may be the null one:
                // return natively.
                if tag == 0 && args.len() == 1 && t == 0 {
                    $done!($vm.val(args[0], &mut $env));
                }
                let (m, ncap) = $vm.method(method_of(t) + tag as usize);
                let (vals, frame) = $vm.transfer(args, &mut $env);
                match frame {
                    None => {
                        $p = m;
                        $set_env!($vm.enter_method(t, m, ncap, vals));
                        $vm.safe_point($p, &mut $env);
                    }
                    Some(fr) => $wait_frame!(fr, Target::Method(t, m, ncap), vals),
                }
                $enter!();
            }
            Ins::Call(i) => {
                let c = &$code.calls[i as usize];
                let mut vals: Args = c
                    .args
                    .iter()
                    .map(|a| V::Val($vm.val(*a, &mut $env)))
                    .collect();
                vals.push(V::Val(0));
                let target = match c.callee {
                    Callee::Proc(q) => Target::Def($vm.target(q)),
                    Callee::Method { target, tag } => {
                        let t = $vm.val(target, &mut $env);
                        if t == 0 {
                            fail("the null continuation was given a frame");
                        }
                        let (m, ncap) = $vm.method(method_of(t) + tag as usize);
                        Target::Method(t, m, ncap)
                    }
                };
                $wait_held!(c, target, vals);
            }
            Ins::Prim(i) => {
                let x = &$code.prims[i as usize];
                let v = $vm.op(&x.op, &x.args, &mut $env);
                if let Some(r) = x.result {
                    $env[r as usize] = V::Val(v);
                }
                $go!($pc + 1);
            }
            Ins::Bin { op, a, b, dst } => {
                let r = op.run($vm.word(a.into(), &mut $env), $vm.word(b.into(), &mut $env));
                $env[dst as usize] = V::Val(r);
                $go!($pc + 1);
            }
            Ins::BinK { op, a, dst, k } => {
                let r = op.run($vm.word(a.into(), &mut $env), $code.consts[k as usize]);
                $env[dst as usize] = V::Val(r);
                $go!($pc + 1);
            }
            Ins::BinI { op, a, dst, k } => {
                let r = op.run($vm.word(a.into(), &mut $env), k as i64 as Word);
                $env[dst as usize] = V::Val(r);
                $go!($pc + 1);
            }
            Ins::IfBin { op, a, b, no } => {
                let r = op.run($vm.word(a.into(), &mut $env), $vm.word(b.into(), &mut $env));
                $go!(if r != 0 { $pc + 1 } else { no as usize });
            }
            Ins::IfBinI { op, a, k, no } => {
                let r = op.run($vm.word(a.into(), &mut $env), k as i64 as Word);
                $go!(if r != 0 { $pc + 1 } else { no as usize });
            }
            Ins::Branch(t) => {
                let t = &$code.tests[t as usize];
                $go!(if $vm.test(&t.test, &t.args, &mut $env) {
                    $pc + 1
                } else {
                    t.no as usize
                });
            }
            Ins::Wide(w) => match $code.wides[w as usize] {
                WideI::Share { s, d: dd } => {
                    let v = $vm.val(s, &mut $env);
                    $vm.share(v, d(dd, &$env));
                    $go!($pc + 1);
                }
                WideI::Erase { s, d: dd } => {
                    $vm.erase_slot(s, dd, &mut $env);
                    $go!($pc + 1);
                }
                WideI::Bin { op, a, b, dst } => {
                    let r = op.run($vm.word(a, &mut $env), $vm.word(b, &mut $env));
                    $env[dst as usize] = V::Val(r);
                    $go!($pc + 1);
                }
                WideI::BinK { op, a, k, dst } => {
                    let r = op.run($vm.word(a, &mut $env), k);
                    $env[dst as usize] = V::Val(r);
                    $go!($pc + 1);
                }
                WideI::IfBin { op, a, b, no } => {
                    let r = op.run($vm.word(a, &mut $env), $vm.word(b, &mut $env));
                    $go!(if r != 0 { $pc + 1 } else { no as usize });
                }
                WideI::IfBinK { op, a, k, no } => {
                    let r = op.run($vm.word(a, &mut $env), k);
                    $go!(if r != 0 { $pc + 1 } else { no as usize });
                }
            },
            Ins::Segment(i) => {
                let sg = &$code.segments[i as usize];
                let vals: Words = sg
                    .captures
                    .iter()
                    .map(|u| $vm.val(u.s, &mut $env))
                    .collect();
                let descs: Descs = sg.captures.iter().map(|u| d(u.d, &$env)).collect();
                let packed = build_closure(sg.base, &vals, &descs);
                let a = $vm.val(sg.arg.s, &mut $env);
                $done!(match sg.prim {
                    Prim::Enter => meadow_silo::segments::meadow_enter(a, packed),
                    Prim::Detach => meadow_silo::segments::meadow_detach(a, packed),
                    _ => meadow_silo::segments::meadow_reattach(a, packed),
                });
            }
            Ins::Error(i) => fail($code.errors[i as usize]),
        }
    };
}

/// What a slot holds: a word, a frame not built, or nothing yet. Laid out as
/// a tag word and then the word or the frame's pointer, which compiled code
/// reads and writes (`crate::jit`).
#[derive(Default)]
#[repr(u64)]
enum V {
    #[default]
    Empty,
    Val(Word),
    Frame(Box<Frame>),
}

// What compiled code takes a slot to be: two words, the tag and then the
// word or the frame. The tags are checked where the JIT is (`jit::tests`).
const _: () = assert!(std::mem::size_of::<V>() == 16);

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
    /// A top-level value's cache, by a reload's number for it: what the
    /// code says, after those numbered before it.
    Global(u32),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// How a procedure's or a method's number says which segment it is in: the
/// bits above these.
const SEG: u32 = 22;
const LOCAL: u32 = (1 << SEG) - 1;
/// How many loads a program can take: its first, and its reloads.
const SEGMENTS: usize = 1024;
/// How many definitions, over every load.
const DEFS: usize = 1 << 18;

/// One load's code: what the program's first image made, or what a reload
/// added (`docs/RUNTIME-MERGE.md`, "Hot loading"). Nothing in one changes
/// once it is in place, and nothing moves: code already running, and the
/// closures made by it, go on in the segment they began in.
struct Segment {
    program: &'static Program,
    procs: Vec<Proc>,
    code: Code,
    /// Every method, by its number -- what a closure's header holds -- and
    /// how many of its parameters are the closure's captures.
    methods: Vec<(u32, usize)>,
    /// What the JIT has compiled of it.
    jit: Box<jit::Table>,
    /// What its code was compiled from, and how it was numbered: what the
    /// JIT's second tier lowers a definition from again (`tier2`).
    lowered: Lowered,
}

/// A load's linear program, and how loading numbered what is in it: each
/// definition's block, by its place among the load's, and its slot; each
/// `new`'s methods, by the address of the `new` (or of the stack-segment
/// primitive's body whose code it packs); the constructors' tags, the
/// interned names and the top-level values' caches.
pub(super) struct Lowered {
    pub(super) blocks: Vec<(Label, &'static LBlock)>,
    pub(super) slots: HashMap<Label, u32>,
    pub(super) bases: HashMap<usize, u32>,
    pub(super) retag: Vec<u32>,
    pub(super) syms: HashMap<InternedString, usize>,
    pub(super) global_base: u32,
}

/// What loading keeps from one load to the next: how the program running
/// numbers what a new build of it must number alike.
#[derive(Clone, Default)]
struct LoadState {
    /// The interned names, numbered once for every load.
    syms: HashMap<InternedString, usize>,
    /// The slot of each definition a new build may have a new version of, by
    /// what it is: see [`def_keys`].
    slots: HashMap<String, u32>,
    segments: usize,
    /// How many slots are in use.
    defs: usize,
    /// Every constructor's tag, by what it is -- its name, and its type's
    /// version where the program says (`Program::ctor_types`) -- numbered
    /// once for every load; and the next.
    tags: HashMap<(InternedString, Option<InternedString>), u32>,
    next_tag: u32,
    /// Each tag's constructor, and its named fields: a constructor's
    /// versions are told apart by their tags, and may have other fields.
    tag_names: HashMap<u32, InternedString>,
    tag_fields: HashMap<u32, Vec<InternedString>>,
    roles: HashMap<meadow_axcut::Role, InternedString>,
    /// Whether the running program was built to take a new build of itself:
    /// what a reload finds its definitions and constructors by is only
    /// written down then (`meadow_seq::lower_program_with`).
    reloadable: bool,
    /// How many top-level values' caches are numbered: each load's are
    /// numbered after those before it, so that its code computes its own.
    globals: u32,
    /// Whether some load numbered a cache by other than a literal, which
    /// leaves how many there are unknown, and a reload unable to number its
    /// own clear of them.
    opaque_globals: bool,
    config: meadow_axcut::config::Config,
}

impl LoadState {
    fn fresh_slot(&mut self) -> Result<u32, String> {
        if self.defs >= DEFS {
            return Err("more definitions than the program can hold".into());
        }
        self.defs += 1;
        Ok(self.defs as u32 - 1)
    }

    /// Each of `program`'s tags, by its own number, as the program running
    /// numbers the constructor: the first load's as they are, a reload's by
    /// what the constructor is, and those new to it after the rest.
    ///
    /// What a constructor is includes its type's version (see
    /// `meadow_core::versions`): a constructor of a type the new build
    /// changed is a new one, with a tag of its own, so that values made by
    /// the old build and by the new are told apart -- and only the code made
    /// for each takes it, since every definition whose type mentions it is a
    /// new definition too. A reload is refused that changes a constructor the
    /// runtime builds by name -- the roles', and `meadow_rt::roles::RECORDS`
    /// -- which it could not tell which build's it meant.
    fn tags_of(&mut self, program: &Program) -> Result<Vec<u32>, String> {
        let size = program.tags.values().max().map_or(0, |t| *t as usize + 1);
        let id = |n: &InternedString| (*n, program.ctor_types.get(n).copied());
        let fields_of = |n: &InternedString| program.ctor_fields.get(n).cloned();
        if self.segments == 0 {
            for (n, t) in &program.tags {
                self.tags.insert(id(n), *t);
                self.tag_names.insert(*t, *n);
                if let Some(fs) = fields_of(n) {
                    self.tag_fields.insert(*t, fs);
                }
            }
            self.next_tag = size as u32;
            self.roles = program.roles.clone();
            self.reloadable = !program.types.is_empty();
            return Ok((0..size as u32).collect());
        }
        if program.roles != self.roles {
            return Err("its built-in types are other constructors than they were".into());
        }
        let by_name = |n: &InternedString| {
            program.roles.values().any(|r| r == n) || meadow_rt::roles::RECORDS.contains(&&**n)
        };
        let mut by: Vec<(u32, InternedString)> =
            program.tags.iter().map(|(n, t)| (*t, *n)).collect();
        by.sort();
        let mut retag = vec![u32::MAX; size];
        for (t, n) in by {
            let key = id(&n);
            if let Some(had) = self.tags.get(&key) {
                retag[t as usize] = *had;
                continue;
            }
            // New to the program, or of a type this build changed.
            if by_name(&n) && self.tags.keys().any(|(m, _)| *m == n) {
                return Err(format!(
                    "constructor `{}`, which the runtime builds by name, changed",
                    meadow_rt::ctor_spelling(&n)
                ));
            }
            let tag = self.next_tag;
            self.next_tag += 1;
            self.tags.insert(key, tag);
            self.tag_names.insert(tag, n);
            if let Some(fs) = fields_of(&n) {
                self.tag_fields.insert(tag, fs);
            }
            retag[t as usize] = tag;
        }
        Ok(retag)
    }
}

/// The program, ready to run.
pub struct Vm {
    segs: Box<[OnceLock<Segment>]>,
    /// The version of each definition a call by name runs: its slot's
    /// procedure, which a reload replaces.
    defs: Box<[std::sync::atomic::AtomicU32]>,
    /// Where it begins, by slot: its entry, or each of its tests.
    entries: Vec<u32>,
    is_tests: bool,
    state: std::sync::Mutex<LoadState>,
    tracing: bool,
    /// Whether blocks move (`--gc generational`): then what a record holds
    /// is read back after the collection, or the call, it was made for.
    moving: bool,
    /// Whether a debugger watches: then every activation keeps a record, and
    /// every instruction is a place it may stop.
    debugging: bool,
    /// Whether a call that begins with too little stack left runs on a new
    /// piece of it: see `meadow_silo::stack`.
    growable: bool,
    /// Whether a frame waiting on a call puts what it holds on the shadow
    /// chain: when continuations can be abandoned, or memory is traced.
    shadow: bool,
    /// Whether frames live on the interpreter's own stack of them rather than
    /// on the native stack (`--stack heap`): see [`Resume`].
    heap_frames: bool,
    /// The JIT, when it was asked for: see [`jit`].
    jit: Option<Box<jit::Jit>>,
}

/// Under `--stack heap`, the calls waiting on the loop that made them: the
/// frames ([`Resume`]); the slots of their callees, each boxed where it does
/// not move, innermost last -- the first `depth` of them, the rest kept to use
/// again; and the records of what they hold.
#[derive(Default)]
struct Heap<'a> {
    frames: Vec<Resume<'a>>,
    boxes: Vec<Box<Env>>,
    depth: usize,
    records: Records,
}

/// Under `--stack heap`, the words of the records of the calls waiting, kept
/// in chunks that do not move -- made and given up in a stack's order, as the
/// calls are -- so that a record on the shadow chain costs no allocation.
#[derive(Default)]
struct Records {
    chunks: Vec<Box<[i64]>>,
    chunk: usize,
    at: usize,
}

/// How many words a chunk of [`Records`] has, at least.
const RECORDS_CHUNK: usize = 4096;

/// A record [`Records`] holds: where its words are, and where the stack of
/// them was before it.
struct Rec {
    at: *mut i64,
    len: usize,
    mark: (usize, usize),
}

impl Rec {
    fn words(&self) -> &[i64] {
        // Safety: the record's words, which stay where they are until it is
        // given up.
        unsafe { std::slice::from_raw_parts(self.at, self.len) }
    }
}

impl Records {
    /// A record of `words`, on top.
    fn push(&mut self, words: &[i64]) -> Rec {
        let mark = (self.chunk, self.at);
        let n = words.len();
        if self.chunks.is_empty() || self.at + n > self.chunks[self.chunk].len() {
            if !self.chunks.is_empty() {
                self.chunk += 1;
            }
            self.at = 0;
            if self.chunk == self.chunks.len() {
                self.chunks
                    .push(vec![0; RECORDS_CHUNK.max(n)].into_boxed_slice());
            } else if self.chunks[self.chunk].len() < n {
                self.chunks[self.chunk] = vec![0; n].into_boxed_slice();
            }
        }
        let room = &mut self.chunks[self.chunk][self.at..self.at + n];
        room.copy_from_slice(words);
        self.at += n;
        Rec {
            at: room.as_mut_ptr(),
            len: n,
            mark,
        }
    }

    /// Give up `r`, the record on top.
    fn pop(&mut self, r: &Rec) {
        (self.chunk, self.at) = r.mark;
    }
}

/// What a call goes to: a definition's procedure, or method `m` -- of `ncap`
/// captures -- of the closure `obj`.
#[derive(Clone, Copy)]
enum Target {
    Def(u32),
    Method(Word, u32, usize),
}

/// Under `--stack heap`, a call waiting for its answer, kept by the loop that
/// made it rather than by a native call: what it goes on with when the
/// callee returns to the null continuation. The rest of a procedure that
/// made the call itself -- its procedure, the pc after the call, its slots,
/// which stay where they are while the callee has slots of its own, and
/// where the answer goes -- or a frame not built that was handed the call as
/// its continuation. With, where frames are recorded, the record of what it
/// holds, on the shadow chain while it waits.
enum Resume<'a> {
    At {
        proc: u32,
        pc: usize,
        answer: Slot,
        slots: *mut Env,
        held: &'a [(U, Hold)],
        rec: Option<Rec>,
    },
    Frame {
        fr: Box<Frame>,
        rec: Option<Rec>,
    },
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
    // An interpreter is built for one layout, as its runtime is.
    if image.config.layout.description() != heap::LAYOUT {
        return Err(format!(
            "the program is built for the `{}` layout, and this interpreter for the `{}` one: \
             run it with the interpreter built for its layout",
            image.config.layout.name(),
            heap::LAYOUT.name
        ));
    }
    image.config.check()?;
    let program: &'static Program = Box::leak(Box::new(image.program));
    let is_tests = !image.tests.is_empty();
    let entries = if is_tests {
        image.tests
    } else {
        vec![program.entry.ok_or("the program has no entry point")?]
    };
    // What a test answers is not printed; what the program does is.
    let result = match (is_tests, program.entry) {
        (false, Some(e)) => program
            .results
            .get(&e)
            .and_then(|r| r.desc())
            .unwrap_or(desc::ANY),
        _ => desc::ANY,
    };
    let mut state = LoadState {
        config: image.config,
        ..LoadState::default()
    };
    let c = compile(program, image.linear, 0, &mut state)?;
    let entries = entries
        .iter()
        .map(|l| {
            c.slots
                .get(l)
                .copied()
                .ok_or("an entry the program does not have")
        })
        .collect::<Result<Vec<u32>, _>>()?;
    let c_threads = c.threads;
    let tracing = image.config.memory.traced();
    // Blocks that move: what the interpreter can follow, holding every
    // reference where the collector rewrites it (see `Vm::moving`).
    let moving = image.config.memory == meadow_axcut::config::Memory::Generational;
    let growable = image.config.stack == meadow_axcut::config::Stack::Growable;
    // What loading made of the program, for whoever is making it faster.
    if std::env::var_os("MEADOW_VM_STATS").is_some() {
        let seg = &c.segment;
        eprintln!(
            "vm: loaded in {:?}; {} procedures, {} methods, {} instructions; {:?}",
            began.elapsed(),
            seg.procs.len(),
            seg.methods.len(),
            seg.code.ins.len(),
            seg.code.census()
        );
        let slots: Vec<usize> = seg.procs.iter().map(|p| p.slots).collect();
        let over = slots.iter().filter(|n| **n > 16).count();
        eprintln!(
            "vm: {over} of {} procedures have over 16 slots; the most is {}",
            slots.len(),
            slots.iter().max().unwrap_or(&0)
        );
    }
    let defs: Box<[std::sync::atomic::AtomicU32]> = (0..DEFS)
        .map(|_| std::sync::atomic::AtomicU32::new(u32::MAX))
        .collect();
    for (slot, proc) in &c.defs {
        defs[*slot as usize].store(*proc, Ordering::Relaxed);
    }
    let segs: Box<[OnceLock<Segment>]> = (0..SEGMENTS).map(|_| OnceLock::new()).collect();
    let _ = segs[0].set(c.segment);
    state.segments = 1;
    let names = names_of(&state);
    let vm = Vm {
        segs,
        defs,
        entries,
        is_tests,
        state: std::sync::Mutex::new(state),
        tracing,
        moving,
        // A profile reads the debugger's records of the activations.
        debugging: DEBUGGING.load(Ordering::Relaxed) || profile::on(),
        growable,
        shadow: tracing || c.detaches,
        heap_frames: image.config.stack == meadow_axcut::config::Stack::Heap,
        // Compiled code calls natively, which frames on the interpreter's own
        // stack are there not to.
        jit: if image.config.stack == meadow_axcut::config::Stack::Heap {
            None
        } else {
            jit::Jit::from_env(c_threads)?.map(Box::new)
        },
    };
    // Its records kept from the start: see `meadow_silo::coverage::recording`.
    if vm.debugging {
        meadow_silo::coverage::record();
    }
    // Safety: set before the runtime starts, which reads them.
    unsafe {
        crate::meadow_threaded = u8::from(c.threads);
        crate::meadow_cycles = u8::from(!tracing);
        crate::meadow_gc = if moving { 2 } else { u8::from(tracing) };
        crate::meadow_result_desc = result;
    }
    meadow_silo::show::install(names);
    VM.set(vm)
        .map_err(|_| "a program was loaded twice".to_string())
}

impl Vm {
    /// Load `image`, a new build of the program running, beside it, while it
    /// runs (`docs/RUNTIME-MERGE.md`, "Hot loading"): its code goes in a
    /// segment of its own, and each definition it has a new version of is
    /// called by that from now on. What is running already goes on as it
    /// was -- a call under way, a closure or a continuation already made,
    /// runs the code it was made of -- and only what calls a definition by
    /// name after this finds the new version. Its constructors are numbered
    /// as the running program numbers them -- those of a type it changed
    /// anew, since a definition that takes one is a new definition too (see
    /// `meadow_core::versions`) -- and its top-level values are computed
    /// again, by its own code, the first time it asks for them.
    ///
    /// Refused, and nothing changed, when what is already made cannot be
    /// what the new code expects: a constructor the runtime builds by name
    /// that changed, a build with nothing versioned (`--no-reload`), or a
    /// program built to run otherwise. Answers what it replaced and added.
    pub fn reload(&self, image: meadow_axcut::image::Image) -> Result<String, String> {
        let mut held = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if image.config != held.config {
            return Err("it was built to manage memory or the stack otherwise".into());
        }
        if !image.tests.is_empty() {
            return Err("it is a program of tests".into());
        }
        if !held.reloadable {
            return Err("the program running was built without hot loading (`--no-reload`)".into());
        }
        if image.program.types.is_empty() {
            return Err("it was built without hot loading (`--no-reload`)".into());
        }
        let seg = held.segments;
        if seg >= SEGMENTS {
            return Err("the program has been reloaded as often as it can be".into());
        }
        let program: &'static Program = Box::leak(Box::new(image.program));
        let mut state = held.clone();
        let mut made = None;
        meadow_silo::show::reinstall(|extra| -> Result<_, String> {
            // The labels the runtime numbered after the program's own are
            // the program's from now on, numbered as they were.
            let base = state.syms.len();
            for (j, s) in extra.iter().enumerate() {
                state
                    .syms
                    .insert(InternedString::from(s.as_str()), base + j);
            }
            let c = compile(program, image.linear, seg as u32, &mut state)?;
            // Safety: read only; the runtime set it when it started.
            if c.threads && unsafe { crate::meadow_threaded } == 0 {
                return Err(
                    "it starts threads, which the program running was not set up for".into(),
                );
            }
            if c.detaches && !self.shadow {
                return Err(
                    "it detaches continuations, which the program running was not set up for"
                        .into(),
                );
            }
            let names = names_of(&state);
            made = Some(c);
            Ok(names)
        })?;
        let c = made.expect("compiled");
        let _ = self.segs[seg].set(c.segment);
        // Under a debugger, which is told of the new code before anything
        // calls it.
        if let Some(d) = debug::debugger() {
            d.loaded(seg);
        }
        // The segment in place first: a slot is read by whatever calls it.
        for (slot, proc) in &c.defs {
            self.defs[*slot as usize].store(*proc, Ordering::Release);
            // What compiled code calls by name is what the slot holds now.
            if let Some(t2) = self.jit.as_ref().and_then(|j| j.tier2.as_ref()) {
                t2.forget_slot(*slot);
            }
        }
        state.segments = seg + 1;
        *held = state;
        Ok(format!(
            "{} definitions replaced, {} added",
            c.replaced,
            c.defs.len() - c.replaced
        ))
    }
}

/// A block no person wrote -- a join point, a group of local functions, the
/// search a `perform` makes, which the front end names `<join>`,
/// `<letrec>` and `<perform E.op>` -- named after the definition whose code
/// it is: the one that jumps to it, through others like it if need be.
/// `Fib.fib/<join>`, then, which is what a debugger's frames and a profile's
/// samples call it, as Glade's call such code by the function it is in.
fn name_blocks(
    blocks: &[(Label, String, &'static LBlock)],
    owned: &[std::ops::Range<usize>],
    jumps: &[(usize, Label)],
    procs: &mut [Option<Proc>],
) {
    let made = |name: &str| name.rsplit('.').next().is_some_and(|n| n.starts_with('<'));
    let index: HashMap<Label, usize> = blocks
        .iter()
        .enumerate()
        .map(|(i, (l, ..))| (*l, i))
        .collect();
    let mut from: HashMap<usize, Vec<usize>> = HashMap::new();
    for (caller, label) in jumps {
        if let Some(callee) = index.get(label)
            && *callee != *caller
        {
            from.entry(*callee).or_default().push(*caller);
        }
    }
    for (i, (_, name, _)) in blocks.iter().enumerate() {
        if !made(name) {
            continue;
        }
        // The nearest definition a person wrote that jumps here.
        let mut seen = std::collections::HashSet::from([i]);
        let mut frontier = vec![i];
        let mut found = None;
        'search: while !frontier.is_empty() {
            let mut next = Vec::new();
            for at in frontier {
                for caller in from.get(&at).into_iter().flatten() {
                    if !made(&blocks[*caller].1) {
                        found = Some(*caller);
                        break 'search;
                    }
                    if seen.insert(*caller) {
                        next.push(*caller);
                    }
                }
            }
            frontier = next;
        }
        let Some(parent) = found else { continue };
        let own = name.rsplit('.').next().unwrap_or(name);
        let renamed = format!("{}/{own}", blocks[parent].1);
        for p in std::iter::once(i).chain(owned[i].clone()) {
            if let Some(proc) = procs[p].as_mut()
                && let Some(rest) = proc.name.strip_prefix(name.as_str())
            {
                proc.name = format!("{renamed}{rest}");
            }
        }
    }
}

/// One image's code, compiled for segment `seg`.
struct Compiled {
    segment: Segment,
    /// Each definition's slot, by its label.
    slots: HashMap<Label, u32>,
    /// What each of those slots holds now: the definition's procedure.
    defs: Vec<(u32, u32)>,
    /// How many of those slots the program running had already.
    replaced: usize,
    threads: bool,
    detaches: bool,
}

/// Compile `program`, whose linear blocks are `lin`, into segment `seg`,
/// numbering what it shares with the program running -- its definitions'
/// slots, its constructors, its names -- as `state` says, and adding to
/// `state` what it numbers anew.
fn compile(
    program: &'static Program,
    lin: Vec<(usize, Label, LBlock)>,
    seg: u32,
    state: &mut LoadState,
) -> Result<Compiled, String> {
    let retag = state.tags_of(program)?;
    let mut c = Compiler {
        program,
        seg,
        retag,
        globals: state.globals,
        lits: HashMap::new(),
        current: 0,
        jumps: Vec::new(),
        global_count: 0,
        opaque_globals: false,
        news: HashMap::new(),
        codes: HashMap::new(),
        labels: HashMap::new(),
        procs: Vec::new(),
        methods: Vec::new(),
        syms: std::mem::take(&mut state.syms),
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
    // A slot for each definition: the one it is a new version of, if the
    // program running has one, or one of its own. A first load's are
    // numbered as its procedures are.
    let keys = def_keys(program);
    let mut slots = HashMap::new();
    let mut defs = Vec::new();
    let mut replaced = 0;
    for (i, (label, ..)) in blocks.iter().enumerate() {
        let slot = match keys.get(label) {
            Some(k) => match state.slots.get(k) {
                Some(s) => {
                    replaced += 1;
                    *s
                }
                None => {
                    let s = state.fresh_slot()?;
                    state.slots.insert(k.clone(), s);
                    s
                }
            },
            None => state.fresh_slot()?,
        };
        c.labels.insert(*label, slot);
        slots.insert(*label, slot);
        defs.push((slot, c.global(i)));
        c.procs.push(None);
    }
    // Each definition's procedures: its own, and those made inside it.
    let mut owned = Vec::with_capacity(blocks.len());
    for (i, (_, name, b)) in blocks.iter().enumerate() {
        c.current = i;
        let before = c.procs.len();
        let p = c.proc(b, name.clone());
        c.procs[i] = Some(p);
        owned.push(before..c.procs.len());
    }
    name_blocks(&blocks, &owned, &c.jumps, &mut c.procs);
    // A reload's caches numbered clear of the running program's: which
    // they cannot be, where how many it has is not known.
    let caches = c.global_count > 0 || c.opaque_globals;
    if seg != 0 && caches && (state.opaque_globals || c.opaque_globals) {
        return Err(
            "its top-level values cannot be numbered apart from the running program's".into(),
        );
    }
    if c.procs.len() > LOCAL as usize || c.methods.len() > LOCAL as usize {
        return Err("it has more procedures than a segment can hold".into());
    }
    let lowered = Lowered {
        blocks: blocks.iter().map(|(l, _, b)| (*l, *b)).collect(),
        slots: slots.clone(),
        bases: c
            .news
            .iter()
            .map(|(k, (b, _))| (*k, *b))
            .chain(c.codes.iter().map(|(k, (b, _))| (*k, *b)))
            .collect(),
        retag: c.retag.clone(),
        syms: c.syms.clone(),
        global_base: c.globals,
    };
    state.syms = c.syms;
    state.globals += c.global_count;
    state.opaque_globals |= c.opaque_globals;
    let procs: Vec<Proc> = c.procs.into_iter().map(|p| p.expect("compiled")).collect();
    let jit = Box::new(jit::Table::new(&procs, c.code.ins.len(), c.methods.len()));
    Ok(Compiled {
        segment: Segment {
            program,
            procs,
            code: c.code,
            methods: c.methods,
            jit,
            lowered,
        },
        slots,
        defs,
        replaced,
        threads: c.threads,
        detaches: c.detaches,
    })
}

// --- loading ------------------------------------------------------------------

struct Compiler {
    program: &'static Program,
    /// The segment this load's code goes in: the bits above [`LOCAL`] of
    /// every procedure and method it numbers.
    seg: u32,
    /// Each of the program's tags, as the program running numbers it: see
    /// [`LoadState::tags_of`].
    retag: Vec<u32>,
    /// Where this load's top-level values' caches are numbered from, and
    /// how many it numbers: one more than the most a literal names.
    globals: u32,
    global_count: u32,
    /// Whether one is numbered by other than a literal.
    opaque_globals: bool,
    /// The names bound to integer literals: what a cache is numbered by.
    lits: HashMap<Name, i64>,
    /// The definition being compiled, by its place among this load's, and
    /// each jump or direct call one makes to another's label: what a block
    /// no person wrote is named after (see [`name_blocks`]).
    current: usize,
    jumps: Vec<(usize, Label)>,
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
        self.global(self.procs.len() - 1)
    }

    /// This segment's procedure or method `local`, by the number it goes by.
    fn global(&self, local: usize) -> u32 {
        (self.seg << SEG) | local as u32
    }

    /// Tag `t` of this load's program, as the program running numbers it.
    fn tag(&self, t: u32) -> u32 {
        self.retag
            .get(t as usize)
            .copied()
            .filter(|t| *t != u32::MAX)
            .unwrap_or(t)
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
                    tag: self.tag(*tag),
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
                        tag: u64::from(self.tag(a.tag)),
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
                        let made = (self.global(base), procs);
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
                self.jumps.push((self.current, *label));
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
                    proc: {
                        self.jumps.push((self.current, *label));
                        *p
                    },
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
                    let base = self.global(base);
                    self.codes.insert(key, (base, caps.clone()));
                    (base, caps)
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
            Extern::Lit(l) => {
                if let (Lit::Int(n), Some(r)) = (l, result) {
                    self.lits.insert(r, *n);
                }
                Op::Lit(self.literal(l))
            }
            // A top-level value's cache, numbered by a literal: by this
            // load's own numbers, after the loads before it.
            Extern::Prim(p @ (Prim::GlobalReady | Prim::GlobalGet | Prim::GlobalSet)) => {
                match args.first().and_then(|a| self.lits.get(a)) {
                    Some(i) => self.global_count = self.global_count.max(*i as u32 + 1),
                    None => self.opaque_globals = true,
                }
                if self.globals == 0 {
                    Op::Prim(*p, self.kind(*p, args, result, scope))
                } else {
                    Op::Prim(*p, PK::Global(self.globals))
                }
            }
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
    /// The segment procedure or method `id` is in.
    #[inline(always)]
    fn seg(&self, id: u32) -> &Segment {
        // Safety: a number is only made in a segment's compiling, which puts
        // the segment in place before any code holding the number runs.
        unsafe {
            self.segs
                .get_unchecked((id >> SEG) as usize)
                .get()
                .unwrap_unchecked()
        }
    }

    #[inline(always)]
    fn proc(&self, id: u32) -> &Proc {
        // Safety: as above, a procedure's number is one its segment made.
        unsafe { self.seg(id).procs.get_unchecked((id & LOCAL) as usize) }
    }

    /// Method `id`: its procedure, and how many captures it loads.
    #[inline(always)]
    fn method(&self, id: usize) -> (u32, usize) {
        let id = id as u32;
        // Safety: as above, a closure's method is one its segment made.
        unsafe { *self.seg(id).methods.get_unchecked((id & LOCAL) as usize) }
    }

    /// Method `id`, if there is one: for a debugger, which reads words that
    /// may be anything.
    fn method_checked(&self, id: usize) -> Option<(u32, usize)> {
        let id = u32::try_from(id).ok()?;
        let seg = self.segs.get((id >> SEG) as usize)?.get()?;
        seg.methods.get((id & LOCAL) as usize).copied()
    }

    /// The version of the definition in `slot` a call by name runs now.
    #[inline(always)]
    fn target(&self, slot: u32) -> u32 {
        self.defs[slot as usize].load(Ordering::Acquire)
    }

    /// The first load's segment: what a debugger shows.
    fn first(&self) -> &Segment {
        self.seg(0)
    }

    /// The code procedure `p` is in, and where it begins.
    #[inline(always)]
    fn entry(&self, p: u32) -> (&Code, usize) {
        let s = self.seg(p);
        // Safety: as in `proc`.
        let entry = unsafe { s.procs.get_unchecked((p & LOCAL) as usize).entry };
        (&s.code, entry as usize)
    }

    /// How many tests the program is, or `None` if it is a program.
    pub fn tests(&self) -> Option<usize> {
        self.is_tests.then_some(self.entries.len())
    }

    /// The entry point, given the null continuation: what it answers.
    pub fn run_entry(&self) -> Word {
        let i = CHOSEN.load(Ordering::Relaxed);
        let mut args = Args::new();
        args.push(V::Val(0));
        let p = self.target(self.entries[i]);
        if self.jit.is_some() {
            self.call::<true>(p, args)
        } else {
            self.call::<false>(p, args)
        }
    }

    /// Method `tag` of the closure `obj`, with `args`: entered from the
    /// runtime, as the compiled code's `meadow_invoke1` and `meadow_apply`.
    pub fn invoke(&self, obj: Word, tag: u32, args: &[Word]) -> Word {
        let (p, ncap) = self.method(method_of(obj) + tag as usize);
        let mut env = self.enter_method(obj, p, ncap, args.iter().map(|a| V::Val(*a)).collect());
        self.safe_point(p, &mut env);
        if self.jit.is_some() {
            self.exec::<true>(p, &mut env)
        } else {
            self.exec::<false>(p, &mut env)
        }
    }

    /// Procedure `p` with `args`, to its native return: with the JIT if
    /// `JIT`. The slots are this native frame's, and the loop works in them
    /// where they are.
    #[inline(always)]
    fn call<const JIT: bool>(&self, p: u32, args: Args) -> Word {
        let mut env = Env::new();
        self.bind(p, args, &mut env);
        self.safe_point(p, &mut env);
        self.exec::<JIT>(p, &mut env)
    }

    /// `env` made procedure `p`'s, holding `args` as its first slots.
    #[inline(always)]
    fn bind(&self, p: u32, args: Args, env: &mut Env) {
        env.clear();
        env.resize_with(self.proc(p).slots.max(args.len()), V::default);
        for (slot, a) in env.iter_mut().zip(args) {
            *slot = a;
        }
    }

    fn exec<const JIT: bool>(&self, p: u32, env: &mut Env) -> Word {
        // Every call the interpreter makes natively begins here: where a
        // growable program checks it has stack enough left.
        if self.growable && meadow_silo::stack::low() {
            return meadow_silo::stack::grow(move || self.exec::<JIT>(p, env));
        }
        // Under a debugger, a profile, or once the program has asked what it
        // reached: the loop that keeps records, and notes what it runs.
        // With the JIT: the loop that enters compiled code where it can, and
        // records nothing.
        if self.heap_frames {
            return if meadow_silo::coverage::recording() {
                self.run_heap::<true>(p, env)
            } else {
                self.run_heap::<false>(p, env)
            };
        }
        if meadow_silo::coverage::recording() {
            self.run::<true, false, false>(p, env, None)
        } else if JIT {
            self.run::<false, true, false>(p, env, None)
        } else {
            self.run::<false, false, false>(p, env, None)
        }
    }

    /// [`Vm::run`] under `--stack heap`: frames on the loop's own stack of
    /// them ([`Resume`]), and no call made natively.
    fn run_heap<const DEBUG: bool>(&self, p: u32, env: &mut Env) -> Word {
        self.run::<DEBUG, false, true>(p, env, None)
    }

    /// Call `target` with `vals`, to its native return.
    #[inline(always)]
    fn called<const JIT: bool>(&self, target: Target, vals: Args) -> Word {
        match target {
            Target::Def(q) => self.call::<JIT>(q, vals),
            Target::Method(t, m, ncap) => {
                let mut e = self.enter_method(t, m, ncap, vals);
                self.safe_point(m, &mut e);
                self.exec::<JIT>(m, &mut e)
            }
        }
    }

    /// Make `env` the slots of `target` called with `vals`, past its safe
    /// point: the procedure to go on in.
    #[inline(always)]
    fn begin(&self, target: Target, vals: Args, env: &mut Env) -> u32 {
        let p = match target {
            Target::Def(q) => {
                self.bind(q, vals, env);
                q
            }
            Target::Method(t, m, ncap) => {
                self.enter_method_into(t, m, ncap, vals, env);
                m
            }
        };
        self.safe_point(p, env);
        p
    }

    /// What a call that waits holds, on the shadow chain while it waits: what
    /// [`Vm::waiting_in`] records, kept where it does not move.
    #[inline(always)]
    fn record_held(&self, held: &[(U, Hold)], env: &[V], recs: &mut Records) -> Option<Rec> {
        if !self.shadow {
            return None;
        }
        self.record_held_now(held, env, recs)
    }

    #[inline(never)]
    fn record_held_now(&self, held: &[(U, Hold)], env: &[V], recs: &mut Records) -> Option<Rec> {
        let mut rec: SmallVec<[i64; 18]> = SmallVec::new();
        rec.push(meadow_silo::shadow::head() as i64);
        rec.push(0);
        for (u, h) in held {
            hold(&env[u.s as usize], d(u.d, env), *h, &mut rec, self.moving);
        }
        Self::linked(rec, recs)
    }

    /// [`Vm::record_held`], of what frame `fr` holds.
    fn record_frame(&self, fr: &Frame, recs: &mut Records) -> Option<Rec> {
        if !self.shadow {
            return None;
        }
        let mut rec: SmallVec<[i64; 18]> = SmallVec::new();
        rec.push(meadow_silo::shadow::head() as i64);
        rec.push(0);
        held(fr, &mut rec, self.moving);
        Self::linked(rec, recs)
    }

    /// `rec`, a record's words, put in `recs` and made the shadow chain's
    /// head -- unless it holds nothing.
    fn linked(mut rec: SmallVec<[i64; 18]>, recs: &mut Records) -> Option<Rec> {
        if rec.len() == 2 {
            return None;
        }
        rec[1] = (rec.len() as i64 - 2) / 2;
        let r = recs.push(&rec);
        meadow_silo::shadow::set(r.at as Word);
        Some(r)
    }

    /// A call that waited holding `held` of `env`, its slots, is answered:
    /// its record off the chain and given up, and what it holds read back
    /// where a collection moved it.
    #[inline(always)]
    fn unrecord_held(
        &self,
        rec: Option<Rec>,
        held: &[(U, Hold)],
        env: &mut Env,
        recs: &mut Records,
    ) {
        let Some(rec) = rec else { return };
        let words = rec.words();
        meadow_silo::shadow::set(words[0] as Word);
        if self.moving {
            let mut at = 2;
            for (u, h) in held {
                let dd = d(u.d, env);
                unhold(&mut env[u.s as usize], dd, *h, words, &mut at);
            }
        }
        recs.pop(&rec);
    }

    /// [`Vm::unrecord_held`], for frame `fr`.
    #[inline(always)]
    fn unrecord_frame(&self, rec: Option<Rec>, fr: &mut Frame, recs: &mut Records) {
        let Some(rec) = rec else { return };
        let words = rec.words();
        meadow_silo::shadow::set(words[0] as Word);
        if self.moving {
            unheld(fr, words, &mut 2);
        }
        recs.pop(&rec);
    }

    /// The interpreter's loop, from the start of procedure `p`: with a
    /// debugger's record kept and its stops checked when `DEBUG`, which a
    /// program run without one never pays for.
    ///
    /// With `HEAP`, a call that waits for its answer does not call natively:
    /// the caller's [`Resume`] goes on the loop's stack of them, and the loop
    /// goes on in the callee; a return to the null continuation goes on with
    /// the last of them, and leaves the loop only when there is none.
    fn run<'a, const DEBUG: bool, const JIT: bool, const HEAP: bool>(
        &'a self,
        mut p: u32,
        mut env: &mut Env,
        handed: Option<(usize, Heap<'a>)>,
    ) -> Word {
        let (mut code, mut pc) = self.entry(p);
        if JIT {
            match self.jitted(&mut p, env) {
                jit::Jitted::Done(w) => return w,
                jit::Jitted::Entry => (code, pc) = self.entry(p),
                jit::Jitted::At(n) => (code, pc) = (self.entry(p).0, n),
            }
        }
        // Under `HEAP`, the calls waiting: see [`Heap`]. Handed over by a
        // loop that did not keep records, when records began, with the pc
        // it was at.
        let mut heap: Heap<'a> = Heap::default();
        if let Some((at, h)) = handed {
            pc = at;
            heap = h;
        }
        // The record a debugger reads this activation by, on the running
        // segment's chain while it runs.
        let parent = if DEBUG { meadow_silo::frames::top() } else { 0 };
        let base = debug::Act::depth_of(parent) + 1;
        let mut act = debug::Act {
            parent,
            depth: base,
            proc: p,
            pc,
            env: &*env as *const Env,
            frames: std::ptr::null(),
        };
        let _popped = DEBUG.then(|| {
            meadow_silo::frames::set_top(&act as *const debug::Act as Word);
            Popped(parent)
        });
        let watching = if DEBUG { debug::debugger() } else { None };
        loop {
            if DEBUG {
                act.proc = p;
                act.pc = pc;
                if HEAP {
                    // Each frame waiting is a call deeper, as it would be
                    // on the native stack.
                    act.depth = base + heap.frames.len() as u32;
                    act.env = &*env as *const Env;
                    act.frames = &heap.frames as *const Vec<Resume<'a>> as *const ();
                }
                match watching {
                    Some(w) => w.at(&act),
                    None => profile::tick(&act),
                }
                if meadow_silo::coverage::asked() {
                    // A place is a pc of a segment: the first load's are
                    // numbered from nought.
                    meadow_silo::coverage::note((u64::from(p >> SEG) << 32) | pc as u64);
                }
            }
            let vm = self;
            // Records begun, under `HEAP` in the loop that keeps none: the
            // loop that keeps them goes on from here, with the calls waiting.
            macro_rules! handover {
                () => {
                    if HEAP && !DEBUG && meadow_silo::coverage::recording() {
                        let h = std::mem::take(&mut heap);
                        return vm.run::<true, false, true>(p, env, Some((pc, h)));
                    }
                };
            }
            macro_rules! go {
                ($n:expr) => {{
                    pc = $n;
                    continue;
                }};
            }
            macro_rules! enter {
                () => {{
                    if JIT {
                        // Records begun -- a debugger, a profile, coverage --
                        // are kept by the loop that keeps them, which never
                        // enters compiled code.
                        if meadow_silo::coverage::recording() {
                            return vm.run::<true, false, false>(p, env, None);
                        }
                        match vm.jitted(&mut p, env) {
                            jit::Jitted::Done(w) => return w,
                            jit::Jitted::At(n) => {
                                (code, pc) = (vm.entry(p).0, n);
                                continue;
                            }
                            jit::Jitted::Entry => {}
                        }
                    }
                    (code, pc) = vm.entry(p);
                    handover!();
                    continue;
                }};
            }
            macro_rules! done {
                ($w:expr) => {{
                    let w = $w;
                    if HEAP && let Some(r) = heap.frames.pop() {
                        match r {
                            Resume::At {
                                proc,
                                pc: at,
                                answer,
                                slots,
                                held,
                                rec,
                            } => {
                                heap.depth -= 1;
                                // Safety: the caller's slots, which stay where
                                // they are while it waits.
                                env = unsafe { &mut *slots };
                                vm.unrecord_held(rec, held, env, &mut heap.records);
                                env[answer as usize] = V::Val(w);
                                p = proc;
                                pc = at;
                                code = vm.entry(p).0;
                            }
                            Resume::Frame { mut fr, rec } => {
                                vm.unrecord_frame(rec, &mut fr, &mut heap.records);
                                p = vm.enter_frame(fr, V::Val(w), env);
                                (code, pc) = vm.entry(p);
                            }
                        }
                        handover!();
                        continue;
                    }
                    return w;
                }};
            }
            macro_rules! set_env {
                ($e:expr) => {
                    *env = $e
                };
            }
            // A call that waits for its answer: natively, or -- under `HEAP`
            // -- by putting the caller on the stack of frames and going on
            // in the callee.
            macro_rules! wait_frame {
                ($fr:expr, $target:expr, $vals:expr) => {{
                    let fr = $fr;
                    if HEAP {
                        let rec = vm.record_frame(&fr, &mut heap.records);
                        heap.frames.push(Resume::Frame { fr, rec });
                        p = vm.begin($target, $vals, &mut env);
                    } else {
                        let mut fr = fr;
                        let r = vm.waiting(&mut fr, || vm.called::<JIT>($target, $vals));
                        p = vm.enter_frame(fr, V::Val(r), &mut env);
                    }
                }};
            }
            macro_rules! wait_held {
                ($c:expr, $target:expr, $vals:expr) => {{
                    let c = $c;
                    if HEAP {
                        let rec = vm.record_held(&c.held, &env, &mut heap.records);
                        heap.frames.push(Resume::At {
                            proc: p,
                            pc: pc + 1,
                            answer: c.answer,
                            slots: &mut *env as *mut Env,
                            held: &c.held,
                            rec,
                        });
                        // The callee's slots, of their own.
                        if heap.depth == heap.boxes.len() {
                            heap.boxes.push(Box::default());
                        }
                        let b: *mut Env = &mut *heap.boxes[heap.depth];
                        heap.depth += 1;
                        // Safety: a box, which does not move while it is in
                        // `boxes`, and is not used again until the callee
                        // returns.
                        env = unsafe { &mut *b };
                        p = vm.begin($target, $vals, env);
                        (code, pc) = vm.entry(p);
                        handover!();
                        continue;
                    }
                    let r = vm.waiting_in(&c.held, &mut env, || vm.called::<JIT>($target, $vals));
                    env[c.answer as usize] = V::Val(r);
                    pc += 1;
                    continue;
                }};
            }
            instruction!(
                vm, JIT, p, env, code, pc, go, enter, done, set_env, wait_frame, wait_held
            )
        }
    }

    /// Run the instruction at `pc` of `code`, in procedure `*procedure` with
    /// `env`: where to go next, as [`Next`] says. What compiled code asks of
    /// the interpreter for an instruction it does not compile.
    fn step(&self, procedure: &mut u32, mut env: &mut Env, code: &Code, pc: usize) -> Next {
        let vm = self;
        // No instruction reads the procedure it is in: one that enters
        // another says which, and only then is it written back.
        let p: u32;
        macro_rules! go {
            ($n:expr) => {
                return Next::At($n)
            };
        }
        macro_rules! enter {
            () => {{
                *procedure = p;
                return Next::Enter;
            }};
        }
        macro_rules! done {
            ($w:expr) => {
                return Next::Done($w)
            };
        }
        macro_rules! set_env {
            ($e:expr) => {
                *env = $e
            };
        }
        macro_rules! wait_frame {
            ($fr:expr, $target:expr, $vals:expr) => {{
                let mut fr = $fr;
                let r = vm.waiting(&mut fr, || vm.called::<true>($target, $vals));
                p = vm.enter_frame(fr, V::Val(r), env);
            }};
        }
        macro_rules! wait_held {
            ($c:expr, $target:expr, $vals:expr) => {{
                let c = $c;
                let r = vm.waiting_in(&c.held, env, || vm.called::<true>($target, $vals));
                env[c.answer as usize] = V::Val(r);
                return Next::At(pc + 1);
            }};
        }
        instruction!(
            vm, true, p, env, code, pc, go, enter, done, set_env, wait_frame, wait_held
        )
    }

    /// The whole program's code, procedure by procedure, as `--dis` writes it.
    pub fn disassemble(&self) -> String {
        use std::fmt::Write;
        let first = self.first();
        let mut order: Vec<usize> = (0..first.procs.len()).collect();
        order.sort_by_key(|i| first.procs[*i].entry);
        let mut out = String::new();
        for (k, i) in order.iter().enumerate() {
            let pr = &first.procs[*i];
            let end = order
                .get(k + 1)
                .map_or(first.code.ins.len(), |j| first.procs[*j].entry as usize);
            let _ = writeln!(
                out,
                "p{i} {} ({} registers, {} parameters)",
                pr.name,
                pr.slots,
                pr.params.len()
            );
            let mut last = None;
            for pc in pr.entry as usize..end {
                let loc = first.code.locs[pc];
                let at = if loc != last && loc.is_some() {
                    let l = loc.expect("some");
                    format!("  ; source {} at {}", l.source, l.span.start)
                } else {
                    String::new()
                };
                last = loc;
                let _ = writeln!(out, "  {pc:6}  {}{at}", first.code.show(pc));
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
    fn waiting(&self, fr: &mut Frame, call: impl FnOnce() -> Word) -> Word {
        if !self.shadow {
            return call();
        }
        let prev = meadow_silo::shadow::head();
        let mut rec: SmallVec<[i64; 18]> = SmallVec::new();
        rec.push(prev as i64);
        rec.push(0);
        held(fr, &mut rec, self.moving);
        if rec.len() == 2 {
            return call();
        }
        rec[1] = (rec.len() as i64 - 2) / 2;
        meadow_silo::shadow::set(rec.as_mut_ptr() as Word);
        let r = call();
        meadow_silo::shadow::set(prev);
        if self.moving {
            unheld(fr, &rec, &mut 2);
        }
        r
    }

    /// [`Vm::waiting`], for a frame never made: what it would have held is in
    /// `env`, where `held` says.
    #[inline(always)]
    fn waiting_in(&self, held: &[(U, Hold)], env: &mut [V], call: impl FnOnce() -> Word) -> Word {
        if !self.shadow {
            return call();
        }
        let prev = meadow_silo::shadow::head();
        let mut rec: SmallVec<[i64; 18]> = SmallVec::new();
        rec.push(prev as i64);
        rec.push(0);
        for (u, h) in held {
            hold(&env[u.s as usize], d(u.d, env), *h, &mut rec, self.moving);
        }
        if rec.len() == 2 {
            return call();
        }
        rec[1] = (rec.len() as i64 - 2) / 2;
        meadow_silo::shadow::set(rec.as_mut_ptr() as Word);
        let r = call();
        meadow_silo::shadow::set(prev);
        // What it held, where the collection left it: in the same order.
        if self.moving {
            let mut at = 2;
            for (u, h) in held {
                let dd = d(u.d, env);
                unhold(&mut env[u.s as usize], dd, *h, &rec, &mut at);
            }
        }
        r
    }

    /// Under tracing, where a function begins: if a collection is due, run it
    /// with the parameters on the shadow chain.
    #[inline(always)]
    fn safe_point(&self, p: u32, env: &mut [V]) {
        if !self.tracing || meadow_silo::trace::meadow_gc_due.load(Ordering::Relaxed) == 0 {
            return;
        }
        self.collect_at(p, env);
    }

    /// [`Vm::safe_point`] when a collection is due: out of line, since it
    /// seldom is.
    #[inline(never)]
    fn collect_at(&self, p: u32, env: &mut [V]) {
        let prev = meadow_silo::shadow::head();
        let mut rec: Vec<i64> = vec![prev as i64, 0];
        let mut at = Vec::new();
        for (i, pd) in self.proc(p).params.iter().enumerate() {
            if let V::Val(v) = env[i] {
                let dd = d(*pd, env);
                if dd == desc::REF {
                    rec.push(v as i64);
                    rec.push(dd);
                    at.push(i);
                }
            }
        }
        rec[1] = (rec.len() as i64 - 2) / 2;
        meadow_silo::shadow::set(rec.as_mut_ptr() as Word);
        meadow_silo::trace::meadow_gc_poll(meadow_silo::ctx::get());
        meadow_silo::shadow::set(prev);
        // Where the collection left them, if it moved them.
        for (k, i) in at.into_iter().enumerate() {
            env[i] = V::Val(rec[2 + 2 * k] as Word);
        }
    }

    /// A method's entry: its captures loaded out of the closure `obj`, which
    /// is released, and then its arguments.
    fn enter_method(&self, obj: Word, p: u32, ncap: usize, args: Args) -> Env {
        let mut env = Env::new();
        self.enter_method_into(obj, p, ncap, args, &mut env);
        env
    }

    /// [`Vm::enter_method`], into `env`, whatever it held.
    #[inline(always)]
    fn enter_method_into(&self, obj: Word, p: u32, ncap: usize, args: Args, env: &mut Env) {
        let proc = self.proc(p);
        env.clear();
        env.resize_with(proc.slots.max(ncap + args.len()), V::default);
        let loaded = load_fields(obj, ncap);
        for (slot, x) in env.iter_mut().zip(&loaded) {
            *slot = V::Val(*x);
        }
        for (slot, a) in env[ncap..].iter_mut().zip(args) {
            *slot = a;
        }
        let descs: Descs = proc.params[..ncap].iter().map(|pd| d(*pd, env)).collect();
        self.release(obj, &loaded, &descs);
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
            return heap::immediate(u64::from(tag));
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
            PK::Global(base) => {
                let mut ws: Words = vals.into();
                ws[0] = ws[0].wrapping_add(Word::from(base));
                match p {
                    Prim::GlobalReady => rt::meadow_global_ready(ws[0]),
                    Prim::GlobalGet => rt::meadow_global_get(ws[0]),
                    _ => generic(p, &ws, descs),
                }
            }
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
}

/// What each definition is, across builds of the program, for those a new
/// build may have a new version of: its module, its name, its type, and how
/// its parameters and its answer are represented -- a function's copies
/// specialized at different types, and its direct entry point, are told
/// apart by their representations. A reload finds a definition's slot by
/// this, and calls by name that went to the old version go to the new.
///
/// A definition has none, and each build's version is its own, where that
/// could call a version of something else: one no person wrote (a join
/// point, a group of local functions, which only their own definition's
/// code calls), one whose type the front end does not say, and those alike
/// in all of it.
fn def_keys(program: &Program) -> HashMap<Label, String> {
    fn rep(r: Option<&Rep>) -> String {
        match r {
            Some(Rep::Ref) | None => "r".into(),
            Some(Rep::Int) => "i".into(),
            Some(Rep::Float) => "f".into(),
            Some(Rep::Str) => "s".into(),
            Some(Rep::Bits(d)) => format!("b{d}"),
            // A type variable's descriptor: its name is the build's, not the
            // program's.
            Some(Rep::Var(_)) => "v".into(),
            Some(Rep::Unknown) => "?".into(),
        }
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut keys = Vec::new();
    for d in &program.defs {
        let Some(ty) = program.types.get(&d.label) else {
            continue;
        };
        let params: Vec<String> = d
            .block
            .params
            .iter()
            .map(|p| rep(program.reps.get(p)))
            .collect();
        let key = format!(
            "{}.{} : {ty} ({})->{}",
            d.module,
            d.name,
            params.join(","),
            rep(program.results.get(&d.label))
        );
        *seen.entry(key.clone()).or_insert(0) += 1;
        keys.push((d.label, key));
    }
    keys.into_iter().filter(|(_, k)| seen[k] == 1).collect()
}

/// The tables the runtime prints and takes records apart by, as the compiler
/// writes them (`meadow_llvm::emit`'s `header`).
fn names_of(state: &LoadState) -> meadow_silo::show::Names {
    let syms = &state.syms;
    {
        let mut ctors: Vec<(u32, String)> = state
            .tag_names
            .iter()
            .map(|(t, n)| (*t, n.to_string()))
            .collect();
        ctors.sort();
        let count = ctors.last().map_or(0, |(t, _)| *t as usize + 1);
        let mut names = vec![None; count];
        let mut fields = vec![None; count];
        for (t, name) in &ctors {
            names[*t as usize] = Some(name.clone());
            if let Some(fs) = state.tag_fields.get(t) {
                fields[*t as usize] = Some(fs.iter().map(|f| f.to_string()).collect());
            }
        }
        let roles = meadow_rt::roles::Role::ALL
            .into_iter()
            .map(|r| state.roles.get(&r).map(|c| c.to_string()))
            .collect();
        let mut syms: Vec<(usize, InternedString)> = syms.iter().map(|(s, i)| (*i, *s)).collect();
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
/// What `fr` holds, onto a record: what it owns -- and what it borrows too,
/// with `all`, for a collector that moves blocks and must find every
/// reference to one, not only the one that keeps it alive.
fn held(fr: &Frame, out: &mut SmallVec<[i64; 18]>, all: bool) {
    for (c, dd, h) in &fr.captured {
        hold(c, *dd, *h, out, all);
    }
}

/// What a record made by [`held`] says now, back into `fr`: from `at`, in the
/// order it was recorded.
fn unheld(fr: &mut Frame, rec: &[i64], at: &mut usize) {
    for (c, dd, h) in &mut fr.captured {
        unhold(c, *dd, *h, rec, at);
    }
}

/// One of what a frame holds, read back as [`hold`] recorded it, with
/// everything recorded.
fn unhold(c: &mut V, dd: i64, h: Hold, rec: &[i64], at: &mut usize) {
    match (c, h) {
        (V::Empty, _) => {}
        (V::Frame(inner), _) => unheld(inner, rec, at),
        (V::Val(v), Hold::Token) => {
            *v = rec[*at] as Word;
            *at += 2;
        }
        (V::Val(v), _) => {
            if dd == desc::REF || !is_known(dd) {
                *v = rec[*at] as Word;
                *at += 2;
            }
        }
    }
}

/// One of what a frame holds, as [`held`] records it.
fn hold(c: &V, dd: i64, h: Hold, out: &mut SmallVec<[i64; 18]>, all: bool) {
    match (c, h) {
        (_, Hold::Borrowed) if !all => {}
        (V::Empty, _) => {}
        (V::Frame(inner), _) => held(inner, out, all),
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
        return heap::immediate(u64::from(base));
    }
    heap::build(heap::CLOSURE, base, vals, descs)
}

/// Where in the method table the closure `obj`'s methods begin: the number a
/// closure with no block carries, or its block's `meta`.
fn method_of(obj: Word) -> usize {
    heap::meta_of(obj) as usize
}

/// `n` fields of the non-uniform block `v`.
fn load_fields(v: Word, n: usize) -> Words {
    if n == 0 {
        return Words::new();
    }
    let first = heap::LAYOUT.first_field(n, false);
    (0..n).map(|i| heap::word(v, first + i)).collect()
}

/// The references to `v` besides one.
fn count(v: Word) -> u32 {
    heap::count(v)
}

fn set_count(v: Word, rc: u32) {
    heap::set_count(v, rc)
}

/// Write the block at `b` whole: what `heap::build` writes into new memory --
/// its count none beyond the one, its length, kind and `meta`, its fields'
/// descriptors packed as the layout packs them, and its fields.
fn write_block(b: Word, kind: u64, meta: u32, vals: &[Word], descs: &[i64]) {
    let l = heap::LAYOUT;
    let n = vals.len();
    let mut header = l.header(kind, n as u64, u64::from(meta));
    l.pack_descs(descs, |w, bits| {
        if w < l.header_words {
            header[w] |= bits;
        } else {
            heap::set_word(b, w, bits);
        }
    });
    for (i, h) in header.iter().enumerate() {
        heap::set_word(b, i, *h);
    }
    let first = l.first_field(n, false);
    for (i, x) in vals.iter().enumerate() {
        heap::set_word(b, first + i, *x);
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
