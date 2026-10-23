//! **The VM's code**: every procedure's instructions in one array, addressed
//! by a pc, as Glade's are (`docs/RUNTIME-MERGE.md`, "The VM").
//!
//! An instruction is sixteen bytes. The ones a loop is made of -- arithmetic,
//! a compare and branch, a share or an erase -- carry their registers in
//! themselves; the rest name an entry in a table of their kind, where what
//! does not fit (a list of arguments, a switch's arms) is kept. Registers are
//! a procedure's slots, in a window of its own each time it runs.
//!
//! Control never joins: every path through a procedure ends in a jump, an
//! invoke, a return or a failure, as AxCut's statements do. So a branch has
//! the code it takes on after it and names only the pc it goes to otherwise;
//! a switch names where each arm begins; and a call carries on at the next
//! instruction with the answer in its register.
//!
//! Beside the instructions, a line table: for each pc, where in the program's
//! sources it was written, when the build kept that -- a debug build's
//! `Mark`s, which the memory pass passes on.

use super::{B, Callee, D, Hold, I, Op, Slot, Test, U};
use meadow_rt::Loc;
use meadow_silo::heap::Word;

/// A descriptor in a word: a constant -- every one is below sixteen -- or,
/// with the top bit set, the slot whose value is one.
#[derive(Clone, Copy, Debug)]
pub(super) struct Dc(u32);

const SLOT: u32 = 1 << 31;

impl Dc {
    fn of(d: D) -> Dc {
        match d {
            D::K(k) => Dc(u32::try_from(k)
                .ok()
                .filter(|k| *k < SLOT)
                .expect("a small descriptor")),
            D::S(s) => Dc(SLOT | s),
        }
    }

    #[inline(always)]
    pub(super) fn get(self) -> D {
        if self.0 & SLOT == 0 {
            D::K(i64::from(self.0))
        } else {
            D::S(self.0 & !SLOT)
        }
    }
}

/// One instruction.
#[derive(Clone, Copy, Debug)]
pub(super) enum Ins {
    Share {
        s: Slot,
        d: Dc,
    },
    Erase {
        s: Slot,
        d: Dc,
    },
    Rename(u32),
    Let(u32),
    Clean(Slot),
    DropReuse(u32),
    Switch(u32),
    New(u32),
    /// A tail call of a definition.
    Jump(u32),
    /// A tail call of a closure's method, or of a frame, or the return of a
    /// continuation's answer.
    Invoke(u32),
    /// A non-tail call, carrying on at the next instruction.
    Call(u32),
    Prim(u32),
    Bin {
        op: B,
        a: Slot,
        b: Slot,
        dst: Slot,
    },
    BinK {
        op: B,
        a: Slot,
        k: u32,
        dst: Slot,
    },
    /// [`Ins::BinK`] with a constant small enough to be in the instruction.
    BinI {
        op: B,
        a: Slot,
        k: i32,
        dst: Slot,
    },
    /// `op` on two registers: on at the next pc if it holds, at `no` if not.
    IfBin {
        op: B,
        a: Slot,
        b: Slot,
        no: u32,
    },
    IfBinK {
        op: B,
        a: Slot,
        k: u32,
        no: u32,
    },
    IfBinI {
        op: B,
        a: Slot,
        k: i32,
        no: u32,
    },
    Branch {
        test: u32,
        no: u32,
    },
    Segment(u32),
    Error(u32),
}

pub(super) struct LetI {
    pub(super) dst: Slot,
    pub(super) tag: u32,
    pub(super) fields: Vec<U>,
    pub(super) reuse: Option<Slot>,
}

pub(super) struct DropI {
    pub(super) src: Slot,
    pub(super) fields: Vec<D>,
    pub(super) token: Slot,
}

pub(super) struct ArmI {
    pub(super) tag: u64,
    pub(super) fields: Vec<U>,
    pub(super) keep: bool,
    pub(super) reuse: Option<Slot>,
    pub(super) pc: u32,
}

pub(super) struct SwitchI {
    pub(super) scrutinee: Slot,
    pub(super) arms: Vec<ArmI>,
    pub(super) default: u32,
    pub(super) keep_default: bool,
}

pub(super) struct NewI {
    pub(super) dst: Slot,
    pub(super) captures: Vec<U>,
    pub(super) base: u32,
    pub(super) frame: Option<(u32, Vec<Hold>)>,
}

pub(super) struct CallI {
    pub(super) callee: Callee,
    pub(super) args: Vec<Slot>,
    pub(super) held: Vec<(U, Hold)>,
    pub(super) answer: Slot,
}

pub(super) struct PrimI {
    pub(super) op: Op,
    pub(super) args: Vec<U>,
    pub(super) result: Option<Slot>,
}

pub(super) struct TestI {
    pub(super) test: Test,
    pub(super) args: Vec<U>,
}

pub(super) struct SegmentI {
    pub(super) prim: meadow_rt::Prim,
    pub(super) arg: U,
    pub(super) base: u32,
    pub(super) captures: Vec<U>,
}

pub(super) struct JumpI {
    pub(super) proc: u32,
    pub(super) args: Vec<Slot>,
}

pub(super) struct InvokeI {
    pub(super) target: Slot,
    pub(super) tag: u32,
    pub(super) args: Vec<Slot>,
}

/// The instructions, their tables, and where each was written.
#[derive(Default)]
pub(super) struct Code {
    pub(super) ins: Vec<Ins>,
    pub(super) locs: Vec<Option<Loc>>,
    pub(super) consts: Vec<Word>,
    pub(super) renames: Vec<Vec<(Slot, Slot)>>,
    pub(super) lets: Vec<LetI>,
    pub(super) drops: Vec<DropI>,
    pub(super) switches: Vec<SwitchI>,
    pub(super) news: Vec<NewI>,
    pub(super) jumps: Vec<JumpI>,
    pub(super) invokes: Vec<InvokeI>,
    pub(super) calls: Vec<CallI>,
    pub(super) prims: Vec<PrimI>,
    pub(super) tests: Vec<TestI>,
    pub(super) segments: Vec<SegmentI>,
    pub(super) errors: Vec<&'static str>,
}

/// A constant that fits in an instruction: one whose word is a sign-extended
/// 32-bit integer.
fn small(k: Word) -> Option<i32> {
    i32::try_from(k as i64).ok()
}

/// An index into a table, which never holds four billion of anything.
fn at<T>(v: &[T]) -> u32 {
    v.len() as u32
}

impl Code {
    fn push(&mut self, i: Ins, loc: Option<Loc>) -> usize {
        self.ins.push(i);
        self.locs.push(loc);
        self.ins.len() - 1
    }

    fn konst(&mut self, k: Word) -> u32 {
        self.consts.push(k);
        at(&self.consts) - 1
    }

    /// Where the next instruction goes.
    pub(super) fn here(&self) -> u32 {
        at(&self.ins)
    }

    /// `i`, laid out from here: a straight run of instructions to whatever
    /// ends it, and what each branch takes after.
    pub(super) fn emit(&mut self, i: I, mut loc: Option<Loc>) {
        let mut i = i;
        loop {
            i = match i {
                I::Mark(l, rest) => {
                    loc = Some(l);
                    *rest
                }
                I::Share(u, rest) => {
                    self.push(
                        Ins::Share {
                            s: u.s,
                            d: Dc::of(u.d),
                        },
                        loc,
                    );
                    *rest
                }
                I::Erase(u, rest) => {
                    self.push(
                        Ins::Erase {
                            s: u.s,
                            d: Dc::of(u.d),
                        },
                        loc,
                    );
                    *rest
                }
                I::Rename(pairs, rest) => {
                    self.renames.push(pairs);
                    self.push(Ins::Rename(at(&self.renames) - 1), loc);
                    *rest
                }
                I::Let {
                    dst,
                    tag,
                    fields,
                    reuse,
                    rest,
                } => {
                    self.lets.push(LetI {
                        dst,
                        tag,
                        fields,
                        reuse,
                    });
                    self.push(Ins::Let(at(&self.lets) - 1), loc);
                    *rest
                }
                I::Clean(t, rest) => {
                    self.push(Ins::Clean(t), loc);
                    *rest
                }
                I::DropReuse {
                    src,
                    fields,
                    token,
                    rest,
                } => {
                    self.drops.push(DropI { src, fields, token });
                    self.push(Ins::DropReuse(at(&self.drops) - 1), loc);
                    *rest
                }
                I::New {
                    dst,
                    captures,
                    base,
                    frame,
                    rest,
                } => {
                    self.news.push(NewI {
                        dst,
                        captures,
                        base,
                        frame,
                    });
                    self.push(Ins::New(at(&self.news) - 1), loc);
                    *rest
                }
                I::Prim {
                    op,
                    args,
                    result,
                    rest,
                } => {
                    self.prims.push(PrimI { op, args, result });
                    self.push(Ins::Prim(at(&self.prims) - 1), loc);
                    *rest
                }
                I::Bin {
                    op,
                    a,
                    b,
                    dst,
                    rest,
                } => {
                    self.push(Ins::Bin { op, a, b, dst }, loc);
                    *rest
                }
                I::BinK {
                    op,
                    a,
                    k,
                    dst,
                    rest,
                } => {
                    match small(k) {
                        Some(k) => self.push(Ins::BinI { op, a, k, dst }, loc),
                        None => {
                            let k = self.konst(k);
                            self.push(Ins::BinK { op, a, k, dst }, loc)
                        }
                    };
                    *rest
                }
                I::Call {
                    callee,
                    args,
                    held,
                    answer,
                    then,
                } => {
                    self.calls.push(CallI {
                        callee,
                        args,
                        held,
                        answer,
                    });
                    self.push(Ins::Call(at(&self.calls) - 1), loc);
                    *then
                }
                // What branches: the way it holds next, the other after.
                I::IfBin { op, a, b, no, yes } => {
                    let pc = self.push(Ins::IfBin { op, a, b, no: 0 }, loc);
                    self.emit(*yes, loc);
                    let to = self.here();
                    if let Ins::IfBin { no, .. } = &mut self.ins[pc] {
                        *no = to;
                    }
                    *no
                }
                I::IfBinK { op, a, k, no, yes } => {
                    let pc = match small(k) {
                        Some(k) => self.push(Ins::IfBinI { op, a, k, no: 0 }, loc),
                        None => {
                            let k = self.konst(k);
                            self.push(Ins::IfBinK { op, a, k, no: 0 }, loc)
                        }
                    };
                    self.emit(*yes, loc);
                    let to = self.here();
                    if let Ins::IfBinK { no, .. } | Ins::IfBinI { no, .. } = &mut self.ins[pc] {
                        *no = to;
                    }
                    *no
                }
                I::Branch {
                    test,
                    args,
                    no,
                    yes,
                } => {
                    self.tests.push(TestI { test, args });
                    let t = at(&self.tests) - 1;
                    let pc = self.push(Ins::Branch { test: t, no: 0 }, loc);
                    self.emit(*yes, loc);
                    let to = self.here();
                    if let Ins::Branch { no, .. } = &mut self.ins[pc] {
                        *no = to;
                    }
                    *no
                }
                I::Switch {
                    scrutinee,
                    arms,
                    default,
                    keep_default,
                } => {
                    let s = at(&self.switches);
                    self.switches.push(SwitchI {
                        scrutinee,
                        arms: Vec::new(),
                        default: 0,
                        keep_default,
                    });
                    self.push(Ins::Switch(s), loc);
                    let mut laid = Vec::with_capacity(arms.len());
                    for a in arms {
                        let pc = self.here();
                        self.emit(a.body, loc);
                        laid.push(ArmI {
                            tag: a.tag,
                            fields: a.fields,
                            keep: a.keep,
                            reuse: a.reuse,
                            pc,
                        });
                    }
                    let default_at = self.here();
                    let sw = &mut self.switches[s as usize];
                    sw.arms = laid;
                    sw.default = default_at;
                    *default
                }
                // What ends a run.
                I::Jump { proc, args } => {
                    self.jumps.push(JumpI { proc, args });
                    self.push(Ins::Jump(at(&self.jumps) - 1), loc);
                    return;
                }
                I::Invoke { target, tag, args } => {
                    self.invokes.push(InvokeI { target, tag, args });
                    self.push(Ins::Invoke(at(&self.invokes) - 1), loc);
                    return;
                }
                I::Segment {
                    prim,
                    arg,
                    base,
                    captures,
                } => {
                    self.segments.push(SegmentI {
                        prim,
                        arg,
                        base,
                        captures,
                    });
                    self.push(Ins::Segment(at(&self.segments) - 1), loc);
                    return;
                }
                I::Error(msg) => {
                    self.errors.push(msg);
                    self.push(Ins::Error(at(&self.errors) - 1), loc);
                    return;
                }
            }
        }
    }

    /// How many of each instruction there are: for `MEADOW_VM_STATS`.
    pub(super) fn census(&self) -> Vec<(&'static str, usize)> {
        let mut n = std::collections::BTreeMap::new();
        for i in &self.ins {
            *n.entry(name(i)).or_insert(0) += 1;
        }
        n.into_iter().collect()
    }

    // --- the disassembler ---------------------------------------------------------

    fn d(&self, d: Dc) -> String {
        show_d(d.get())
    }

    /// Instruction `pc`, as `meadow-silo-vm --dis` writes it.
    pub(super) fn show(&self, pc: usize) -> String {
        let slots = |v: &[Slot]| {
            v.iter()
                .map(|s| format!("r{s}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let uses = |v: &[U]| v.iter().map(show_u).collect::<Vec<_>>().join(" ");
        let k = |i: u32| format!("#{}", self.consts[i as usize] as i64);
        match self.ins[pc] {
            Ins::Share { s, d } => format!("share r{s}:{}", self.d(d)),
            Ins::Erase { s, d } => format!("erase r{s}:{}", self.d(d)),
            Ins::Rename(i) => format!(
                "rename {}",
                self.renames[i as usize]
                    .iter()
                    .map(|(to, from)| format!("r{to}<-r{from}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Ins::Let(i) => {
                let l = &self.lets[i as usize];
                let reuse = l.reuse.map(|t| format!(" in r{t}")).unwrap_or_default();
                format!("r{} = data #{} [{}]{reuse}", l.dst, l.tag, uses(&l.fields))
            }
            Ins::Clean(t) => format!("clean r{t}"),
            Ins::DropReuse(i) => {
                let x = &self.drops[i as usize];
                format!("r{} = drop-reuse r{}", x.token, x.src)
            }
            Ins::Switch(i) => {
                let s = &self.switches[i as usize];
                let arms = s
                    .arms
                    .iter()
                    .map(|a| {
                        let how = if a.keep {
                            " keep"
                        } else if a.reuse.is_some() {
                            " reuse"
                        } else {
                            ""
                        };
                        format!("#{} [{}]{how} -> {}", a.tag, uses(&a.fields), a.pc)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("switch r{} {{ {arms}, _ -> {} }}", s.scrutinee, s.default)
            }
            Ins::New(i) => {
                let x = &self.news[i as usize];
                let what = if x.frame.is_some() {
                    "frame"
                } else {
                    "closure"
                };
                format!("r{} = {what} m{} [{}]", x.dst, x.base, uses(&x.captures))
            }
            Ins::Jump(i) => {
                let j = &self.jumps[i as usize];
                format!("jump p{} ({})", j.proc, slots(&j.args))
            }
            Ins::Invoke(i) => {
                let v = &self.invokes[i as usize];
                format!("invoke r{}.{} ({})", v.target, v.tag, slots(&v.args))
            }
            Ins::Call(i) => {
                let c = &self.calls[i as usize];
                let callee = match c.callee {
                    Callee::Proc(p) => format!("p{p}"),
                    Callee::Method { target, tag } => format!("r{target}.{tag}"),
                };
                format!("r{} = call {callee} ({})", c.answer, slots(&c.args))
            }
            Ins::Prim(i) => {
                let p = &self.prims[i as usize];
                let to = p.result.map(|r| format!("r{r} = ")).unwrap_or_default();
                format!("{to}{} ({})", show_op(&p.op), uses(&p.args))
            }
            Ins::Bin { op, a, b, dst } => format!("r{dst} = {op:?} r{a} r{b}"),
            Ins::BinK { op, a, k: c, dst } => format!("r{dst} = {op:?} r{a} {}", k(c)),
            Ins::BinI { op, a, k, dst } => format!("r{dst} = {op:?} r{a} #{k}"),
            Ins::IfBin { op, a, b, no } => format!("unless {op:?} r{a} r{b} -> {no}"),
            Ins::IfBinK { op, a, k: c, no } => format!("unless {op:?} r{a} {} -> {no}", k(c)),
            Ins::IfBinI { op, a, k, no } => format!("unless {op:?} r{a} #{k} -> {no}"),
            Ins::Branch { test, no } => {
                let t = &self.tests[test as usize];
                let what = match &t.test {
                    Test::Truth => "true".to_string(),
                    Test::Prim(p, _) | Test::PrimK(p, ..) => format!("{p:?}"),
                };
                format!("unless {what} ({}) -> {no}", uses(&t.args))
            }
            Ins::Segment(i) => {
                let s = &self.segments[i as usize];
                format!(
                    "{:?} {} then m{} [{}]",
                    s.prim,
                    show_u(&s.arg),
                    s.base,
                    uses(&s.captures)
                )
            }
            Ins::Error(i) => format!("error {:?}", self.errors[i as usize]),
        }
    }
}

fn name(i: &Ins) -> &'static str {
    match i {
        Ins::Share { .. } => "share",
        Ins::Erase { .. } => "erase",
        Ins::Rename(_) => "rename",
        Ins::Let(_) => "let",
        Ins::Clean(_) => "clean",
        Ins::DropReuse(_) => "drop_reuse",
        Ins::Switch(_) => "switch",
        Ins::New(_) => "new",
        Ins::Jump(_) => "jump",
        Ins::Invoke(_) => "invoke",
        Ins::Call(_) => "call",
        Ins::Prim(_) => "prim",
        Ins::Bin { .. } | Ins::BinK { .. } | Ins::BinI { .. } => "bin",
        Ins::IfBin { .. } | Ins::IfBinK { .. } | Ins::IfBinI { .. } => "ifbin",
        Ins::Branch { .. } => "branch",
        Ins::Segment(_) => "segment",
        Ins::Error(_) => "error",
    }
}

fn show_d(d: D) -> String {
    match d {
        D::K(k) => format!("{k}"),
        D::S(s) => format!("r{s}"),
    }
}

fn show_u(u: &U) -> String {
    format!("r{}:{}", u.s, show_d(u.d))
}

fn show_op(op: &Op) -> String {
    match op {
        Op::Lit(_) => "lit".into(),
        Op::Prim(p, _) | Op::PrimK(p, ..) => format!("{p:?}"),
        Op::Field(i) => format!("field {i}"),
        Op::Array => "array".into(),
        Op::Record(..) => "record".into(),
        Op::Select(s) => format!("select #{s}"),
        Op::SelectAt(i) => format!("select @{i}"),
        Op::Update(at, of) => format!("update {at}/{of}"),
        Op::Extend(s) => format!("extend #{s}"),
        Op::Native(e, o) => format!("native {}.{}", e.to_string_lossy(), o.to_string_lossy()),
    }
}
