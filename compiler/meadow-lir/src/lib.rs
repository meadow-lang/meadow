//! **The LIR**: the linear program's functions as the machine runs them, with
//! everything a layout and a memory strategy decide written out -- a block's
//! header read at its offset, a count moved, a safe point polled, a record
//! linked into the shadow chain, a barrier called. It sits between linear
//! AxCut (`meadow_axcut::linear`) and the back ends: the LLVM back end
//! (`meadow_llvm`) lowers a program to it and prints it as LLVM IR, and the
//! JIT's second tier compiles it in process. So a layout and a strategy are
//! lowered once, here, for every back end that makes machine code
//! (`docs/RUNTIME-SPEC.md`, LIR-1 to LIR-4).
//!
//! Its instructions are LLVM's, as far as Meadow's code uses them: integer
//! and floating arithmetic, compares, casts, loads and stores at offsets,
//! calls and tail calls, branches and `phi`s, on a few scalar types. An
//! operand is written as LLVM writes it -- a register `%t3`, a number, a
//! global `@name`, `null` -- so that printing one is writing it down, and a
//! back end that is not LLVM reads it back with [`Operand::parse`].
//!
//! [`Inst::Raw`] is an instruction already written as LLVM text: what the
//! lowering has not been taught to say in LIR yet. A back end other than
//! LLVM cannot compile a function holding one.

pub mod parse;

use std::fmt::{self, Write};

/// A value's type: the machine's scalars, as LLVM names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    I1,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    Ptr,
    Void,
}

impl Ty {
    /// The integer type of `bits`.
    pub fn int(bits: u32) -> Ty {
        match bits {
            1 => Ty::I1,
            8 => Ty::I8,
            16 => Ty::I16,
            32 => Ty::I32,
            64 => Ty::I64,
            _ => panic!("no integer type of {bits} bits"),
        }
    }

    /// The type LLVM writes as `text`, if it is one of these.
    pub fn named(text: &str) -> Option<Ty> {
        Some(match text {
            "i1" => Ty::I1,
            "i8" => Ty::I8,
            "i16" => Ty::I16,
            "i32" => Ty::I32,
            "i64" => Ty::I64,
            "float" => Ty::F32,
            "double" => Ty::F64,
            "ptr" => Ty::Ptr,
            "void" => Ty::Void,
            _ => return None,
        })
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Ty::I1 => "i1",
            Ty::I8 => "i8",
            Ty::I16 => "i16",
            Ty::I32 => "i32",
            Ty::I64 => "i64",
            Ty::F32 => "float",
            Ty::F64 => "double",
            Ty::Ptr => "ptr",
            Ty::Void => "void",
        })
    }
}

/// An operand, written as LLVM writes it. See [`Operand`] for what it means.
pub type Val = String;

/// What an operand is, read back from how it is written.
#[derive(Clone, Debug, PartialEq)]
pub enum Operand<'a> {
    /// A register: `%t3`, `%a0`, `%ctx`.
    Reg(&'a str),
    /// An integer constant, `true` and `false` among them.
    Int(i64),
    /// A double, as LLVM writes one exactly: `0x` and its bits.
    Float(u64),
    /// A global's address: `@name`.
    Global(&'a str),
    /// The null pointer.
    Null,
    /// A value nothing reads: `undef` or `poison`.
    Undef,
}

impl<'a> Operand<'a> {
    /// What `v` is, or `None` if it is written in a way the LIR never
    /// writes one.
    pub fn parse(v: &'a str) -> Option<Operand<'a>> {
        Some(match v {
            "true" => Operand::Int(1),
            "false" => Operand::Int(0),
            "null" => Operand::Null,
            "undef" | "poison" => Operand::Undef,
            _ if v.starts_with('%') => Operand::Reg(&v[1..]),
            _ if v.starts_with('@') => Operand::Global(&v[1..]),
            _ if v.starts_with("0x") => Operand::Float(u64::from_str_radix(&v[2..], 16).ok()?),
            _ => Operand::Int(v.parse().ok()?),
        })
    }
}

/// An integer or floating operation on two operands of one type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    SDiv,
    SRem,
    UDiv,
    URem,
    And,
    Or,
    Xor,
    Shl,
    LShr,
    AShr,
    FAdd,
    FSub,
    FMul,
    FDiv,
    FRem,
}

impl BinOp {
    pub fn name(self) -> &'static str {
        match self {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::SDiv => "sdiv",
            BinOp::SRem => "srem",
            BinOp::UDiv => "udiv",
            BinOp::URem => "urem",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::Xor => "xor",
            BinOp::Shl => "shl",
            BinOp::LShr => "lshr",
            BinOp::AShr => "ashr",
            BinOp::FAdd => "fadd",
            BinOp::FSub => "fsub",
            BinOp::FMul => "fmul",
            BinOp::FDiv => "fdiv",
            BinOp::FRem => "frem",
        }
    }

    /// The operation LLVM names `name`.
    pub fn named(name: &str) -> Option<BinOp> {
        [
            BinOp::Add,
            BinOp::Sub,
            BinOp::Mul,
            BinOp::SDiv,
            BinOp::SRem,
            BinOp::UDiv,
            BinOp::URem,
            BinOp::And,
            BinOp::Or,
            BinOp::Xor,
            BinOp::Shl,
            BinOp::LShr,
            BinOp::AShr,
            BinOp::FAdd,
            BinOp::FSub,
            BinOp::FMul,
            BinOp::FDiv,
            BinOp::FRem,
        ]
        .into_iter()
        .find(|o| o.name() == name)
    }
}

/// An integer comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ICmp {
    Eq,
    Ne,
    Slt,
    Sle,
    Sgt,
    Sge,
    Ult,
    Ule,
    Ugt,
    Uge,
}

impl ICmp {
    pub fn name(self) -> &'static str {
        match self {
            ICmp::Eq => "eq",
            ICmp::Ne => "ne",
            ICmp::Slt => "slt",
            ICmp::Sle => "sle",
            ICmp::Sgt => "sgt",
            ICmp::Sge => "sge",
            ICmp::Ult => "ult",
            ICmp::Ule => "ule",
            ICmp::Ugt => "ugt",
            ICmp::Uge => "uge",
        }
    }

    /// The comparison LLVM names `name`.
    pub fn named(name: &str) -> Option<ICmp> {
        [
            ICmp::Eq,
            ICmp::Ne,
            ICmp::Slt,
            ICmp::Sle,
            ICmp::Sgt,
            ICmp::Sge,
            ICmp::Ult,
            ICmp::Ule,
            ICmp::Ugt,
            ICmp::Uge,
        ]
        .into_iter()
        .find(|c| c.name() == name)
    }
}

/// A floating comparison: ordered, so that a NaN compares false, but for
/// `une`, which a NaN makes true.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FCmp {
    Oeq,
    One,
    Olt,
    Ole,
    Ogt,
    Oge,
    Une,
}

impl FCmp {
    pub fn name(self) -> &'static str {
        match self {
            FCmp::Oeq => "oeq",
            FCmp::One => "one",
            FCmp::Olt => "olt",
            FCmp::Ole => "ole",
            FCmp::Ogt => "ogt",
            FCmp::Oge => "oge",
            FCmp::Une => "une",
        }
    }

    /// The comparison LLVM names `name`.
    pub fn named(name: &str) -> Option<FCmp> {
        [
            FCmp::Oeq,
            FCmp::One,
            FCmp::Olt,
            FCmp::Ole,
            FCmp::Ogt,
            FCmp::Oge,
            FCmp::Une,
        ]
        .into_iter()
        .find(|c| c.name() == name)
    }
}

/// A change of type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cast {
    IntToPtr,
    PtrToInt,
    Bitcast,
    ZExt,
    SExt,
    Trunc,
    SIToFP,
    UIToFP,
    FPToSI,
    FPToUI,
    FPExt,
    FPTrunc,
}

impl Cast {
    pub fn name(self) -> &'static str {
        match self {
            Cast::IntToPtr => "inttoptr",
            Cast::PtrToInt => "ptrtoint",
            Cast::Bitcast => "bitcast",
            Cast::ZExt => "zext",
            Cast::SExt => "sext",
            Cast::Trunc => "trunc",
            Cast::SIToFP => "sitofp",
            Cast::UIToFP => "uitofp",
            Cast::FPToSI => "fptosi",
            Cast::FPToUI => "fptoui",
            Cast::FPExt => "fpext",
            Cast::FPTrunc => "fptrunc",
        }
    }

    /// The cast LLVM names `name`.
    pub fn named(name: &str) -> Option<Cast> {
        [
            Cast::IntToPtr,
            Cast::PtrToInt,
            Cast::Bitcast,
            Cast::ZExt,
            Cast::SExt,
            Cast::Trunc,
            Cast::SIToFP,
            Cast::UIToFP,
            Cast::FPToSI,
            Cast::FPToUI,
            Cast::FPExt,
            Cast::FPTrunc,
        ]
        .into_iter()
        .find(|c| c.name() == name)
    }
}

/// One instruction, or the label that begins a block.
#[derive(Clone, Debug, PartialEq)]
pub enum Inst {
    /// Where a block begins.
    Label(String),
    /// `dst = op ty a, b`.
    Bin {
        dst: Val,
        op: BinOp,
        ty: Ty,
        a: Val,
        b: Val,
    },
    /// `dst = icmp cond ty a, b`, an `i1`.
    ICmp {
        dst: Val,
        cond: ICmp,
        ty: Ty,
        a: Val,
        b: Val,
    },
    /// `dst = fcmp cond ty a, b`, an `i1`.
    FCmp {
        dst: Val,
        cond: FCmp,
        ty: Ty,
        a: Val,
        b: Val,
    },
    /// `dst = op from v to to`.
    Cast {
        dst: Val,
        op: Cast,
        from: Ty,
        v: Val,
        to: Ty,
    },
    /// `dst = load ty, ptr p`: volatile where it is read for what another
    /// thread or a signal may write.
    Load {
        dst: Val,
        ty: Ty,
        ptr: Val,
        volatile: bool,
    },
    /// `store ty v, ptr p`.
    Store {
        ty: Ty,
        v: Val,
        ptr: Val,
        volatile: bool,
    },
    /// `dst = getelementptr elem, ptr base, i64 index`: the address of
    /// element `index` of what `base` points at, elements of `elem`.
    Gep {
        dst: Val,
        elem: Ty,
        base: Val,
        index: Val,
    },
    /// `dst = alloca [words x i64]`: room in the frame, for as long as the
    /// function runs.
    Alloca { dst: Val, words: usize },
    /// `dst = select i1 cond, ty a, ty b`.
    Select {
        dst: Val,
        cond: Val,
        ty: Ty,
        a: Val,
        b: Val,
    },
    /// `dst = phi ty [ v, %from ], ...`: the value that came along the edge
    /// from the block taken.
    Phi {
        dst: Val,
        ty: Ty,
        incoming: Vec<(Val, String)>,
    },
    /// `dst = [tail] call [cc] ret callee(args)`: a call, in the C
    /// convention unless `cc` names another, and a tail call if `tail`.
    Call {
        dst: Option<Val>,
        tail: bool,
        cc: Option<String>,
        ret: Ty,
        callee: Val,
        args: Vec<(Ty, Val)>,
    },
    /// `br label %to`.
    Br(String),
    /// `br i1 cond, label %yes, label %no`, with `!prof !n` when `unlikely`
    /// is `Some(n)`: the module's metadata `n`, which says how seldom the
    /// first way is taken.
    CondBr {
        cond: Val,
        yes: String,
        no: String,
        unlikely: Option<u8>,
    },
    /// `switch ty v, label %default [ ty k, label %to ... ]`.
    Switch {
        ty: Ty,
        v: Val,
        default: String,
        cases: Vec<(i64, String)>,
    },
    /// `ret ty v`, or `ret void`.
    Ret { ty: Ty, v: Option<Val> },
    /// `unreachable`.
    Unreachable,
    /// LLVM text the lowering writes as it is: see the module docs.
    Raw(String),
}

impl Inst {
    /// Whether it ends a block.
    pub fn ends_block(&self) -> bool {
        matches!(
            self,
            Inst::Br(_)
                | Inst::CondBr { .. }
                | Inst::Switch { .. }
                | Inst::Ret { .. }
                | Inst::Unreachable
        )
    }
}

impl fmt::Display for Inst {
    /// The instruction as LLVM writes it, on one line and without its
    /// indentation -- a label with its colon.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inst::Label(l) => write!(f, "{l}:"),
            Inst::Bin { dst, op, ty, a, b } => write!(f, "{dst} = {} {ty} {a}, {b}", op.name()),
            Inst::ICmp {
                dst,
                cond,
                ty,
                a,
                b,
            } => {
                write!(f, "{dst} = icmp {} {ty} {a}, {b}", cond.name())
            }
            Inst::FCmp {
                dst,
                cond,
                ty,
                a,
                b,
            } => {
                write!(f, "{dst} = fcmp {} {ty} {a}, {b}", cond.name())
            }
            Inst::Cast {
                dst,
                op,
                from,
                v,
                to,
            } => {
                write!(f, "{dst} = {} {from} {v} to {to}", op.name())
            }
            Inst::Load {
                dst,
                ty,
                ptr,
                volatile,
            } => {
                let v = if *volatile { "volatile " } else { "" };
                write!(f, "{dst} = load {v}{ty}, ptr {ptr}")
            }
            Inst::Store {
                ty,
                v,
                ptr,
                volatile,
            } => {
                let vol = if *volatile { "volatile " } else { "" };
                write!(f, "store {vol}{ty} {v}, ptr {ptr}")
            }
            Inst::Gep {
                dst,
                elem,
                base,
                index,
            } => write!(f, "{dst} = getelementptr {elem}, ptr {base}, i64 {index}"),
            Inst::Alloca { dst, words } => write!(f, "{dst} = alloca [{words} x i64]"),
            Inst::Select {
                dst,
                cond,
                ty,
                a,
                b,
            } => {
                write!(f, "{dst} = select i1 {cond}, {ty} {a}, {ty} {b}")
            }
            Inst::Phi { dst, ty, incoming } => {
                write!(f, "{dst} = phi {ty} ")?;
                for (i, (v, from)) in incoming.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "[ {v}, %{from} ]")?;
                }
                Ok(())
            }
            Inst::Call {
                dst,
                tail,
                cc,
                ret,
                callee,
                args,
            } => {
                if let Some(d) = dst {
                    write!(f, "{d} = ")?;
                }
                if *tail {
                    f.write_str("tail ")?;
                }
                f.write_str("call ")?;
                if let Some(cc) = cc {
                    write!(f, "{cc} ")?;
                }
                write!(f, "{ret} {callee}(")?;
                for (i, (t, a)) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{t} {a}")?;
                }
                f.write_str(")")
            }
            Inst::Br(to) => write!(f, "br label %{to}"),
            Inst::CondBr {
                cond,
                yes,
                no,
                unlikely,
            } => {
                write!(f, "br i1 {cond}, label %{yes}, label %{no}")?;
                if let Some(n) = unlikely {
                    write!(f, ", !prof !{n}")?;
                }
                Ok(())
            }
            Inst::Switch {
                ty,
                v,
                default,
                cases,
            } => {
                write!(f, "switch {ty} {v}, label %{default} [")?;
                for (k, to) in cases {
                    write!(f, " {ty} {k}, label %{to}")?;
                }
                f.write_str(" ]")
            }
            Inst::Ret { ty, v } => match v {
                Some(v) => write!(f, "ret {ty} {v}"),
                None => write!(f, "ret {ty}"),
            },
            Inst::Unreachable => f.write_str("unreachable"),
            Inst::Raw(text) => f.write_str(text),
        }
    }
}

/// A function: what it is called, how it is called, and its code -- the
/// loads that come first (what its caller passed in the spill area), the
/// room it takes in its frame, and its blocks, the first unlabelled.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Func {
    pub name: String,
    /// `hidden`, `internal`, or nothing for the default.
    pub linkage: String,
    /// The calling convention, `ghccc` or `tailcc`; none for C's.
    pub cc: Option<String>,
    pub ret: Option<Ty>,
    pub params: Vec<(Ty, Val)>,
    /// What follows the parameters: `noinline`, `alwaysinline`.
    pub attrs: String,
    /// Whether its first block is written without its `entry:` label, as a
    /// function of one block may be.
    pub unlabelled: bool,
    pub entry: Vec<Inst>,
    pub allocas: Vec<Inst>,
    pub body: Vec<Inst>,
}

impl Func {
    /// The instructions, in order: the entry's, the allocas, the body.
    pub fn insts(&self) -> impl Iterator<Item = &Inst> {
        self.entry.iter().chain(&self.allocas).chain(&self.body)
    }

    /// Whether every instruction is said in LIR, none as LLVM text: what a
    /// back end other than LLVM needs to compile it.
    pub fn is_lir(&self) -> bool {
        !self.insts().any(|i| matches!(i, Inst::Raw(_)))
    }
}

impl fmt::Display for Func {
    /// The function as LLVM IR, ending with a blank line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("define ")?;
        if !self.linkage.is_empty() {
            write!(f, "{} ", self.linkage)?;
        }
        if let Some(cc) = &self.cc {
            write!(f, "{cc} ")?;
        }
        write!(f, "{} {}(", self.ret.unwrap_or(Ty::Void), self.name)?;
        for (i, (t, p)) in self.params.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{t} {p}")?;
        }
        f.write_str(")")?;
        if !self.attrs.is_empty() {
            write!(f, " {}", self.attrs)?;
        }
        f.write_str(if self.unlabelled {
            " {\n"
        } else {
            " {\nentry:\n"
        })?;
        let mut line = String::new();
        for i in self.insts() {
            line.clear();
            match i {
                Inst::Label(_) => {
                    let _ = writeln!(line, "{i}");
                }
                _ => {
                    let _ = writeln!(line, "  {i}");
                }
            }
            f.write_str(&line)?;
        }
        f.write_str("}\n\n")
    }
}

/// A global's type: a scalar, or an array of `len` of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GTy {
    pub elem: Ty,
    pub len: Option<usize>,
}

impl GTy {
    pub fn scalar(elem: Ty) -> GTy {
        GTy { elem, len: None }
    }

    pub fn array(len: usize, elem: Ty) -> GTy {
        GTy {
            elem,
            len: Some(len),
        }
    }
}

impl fmt::Display for GTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.len {
            Some(n) => write!(f, "[{n} x {}]", self.elem),
            None => write!(f, "{}", self.elem),
        }
    }
}

/// What a global holds to begin with.
#[derive(Clone, Debug, PartialEq)]
pub enum Init {
    Int(i64),
    /// Another global's address: `@name`.
    Ref(String),
    Null,
    /// Every byte nought.
    Zero,
    /// An array of bytes, a string's: written as LLVM writes text, `c"..."`.
    Bytes(Vec<u8>),
    /// An array of these, each of the array's element type.
    Elems(Vec<Init>),
}

impl Init {
    fn write(&self, elem: Ty, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Init::Int(n) => write!(f, "{n}"),
            Init::Ref(g) => write!(f, "{g}"),
            Init::Null => f.write_str("null"),
            Init::Zero => f.write_str("zeroinitializer"),
            Init::Bytes(bytes) => {
                f.write_str("c\"")?;
                for b in bytes {
                    match b {
                        b' '..=b'~' if *b != b'"' && *b != b'\\' => {
                            f.write_char(*b as char)?;
                        }
                        _ => write!(f, "\\{b:02X}")?,
                    }
                }
                f.write_str("\"")
            }
            Init::Elems(xs) => {
                f.write_str("[")?;
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{elem} ")?;
                    x.write(elem, f)?;
                }
                f.write_str("]")
            }
        }
    }
}

/// A global: a constant or a variable, defined here with what it holds, or
/// defined elsewhere and named here (`linkage` beginning `external`, and no
/// `init`).
#[derive(Clone, Debug, PartialEq)]
pub struct Global {
    pub name: String,
    pub linkage: String,
    pub constant: bool,
    pub ty: GTy,
    pub init: Option<Init>,
}

impl fmt::Display for Global {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = ", self.name)?;
        if !self.linkage.is_empty() {
            write!(f, "{} ", self.linkage)?;
        }
        write!(
            f,
            "{} {}",
            if self.constant { "constant" } else { "global" },
            self.ty
        )?;
        if let Some(init) = &self.init {
            f.write_str(" ")?;
            init.write(self.ty.elem, f)?;
        }
        Ok(())
    }
}

/// A function defined elsewhere -- the runtime, another unit -- named here.
#[derive(Clone, Debug, PartialEq)]
pub struct Decl {
    pub name: String,
    pub linkage: String,
    pub cc: Option<String>,
    pub ret: Ty,
    pub params: Vec<Ty>,
    /// What follows the parameters: `noreturn`.
    pub attrs: String,
}

impl fmt::Display for Decl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("declare ")?;
        if !self.linkage.is_empty() {
            write!(f, "{} ", self.linkage)?;
        }
        if let Some(cc) = &self.cc {
            write!(f, "{cc} ")?;
        }
        write!(f, "{} {}(", self.ret, self.name)?;
        for (i, t) in self.params.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{t}")?;
        }
        f.write_str(")")?;
        if !self.attrs.is_empty() {
            write!(f, " {}", self.attrs)?;
        }
        Ok(())
    }
}

/// What a module is made of.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Func(Func),
    Decl(Decl),
    Global(Global),
    /// What only LLVM reads, written as it is: a comment, a branch weight's
    /// metadata, a blank line. It means nothing to the code.
    Note(String),
}

impl fmt::Display for Item {
    /// The item as LLVM IR: a definition ends with a blank line, a
    /// declaration or a global with the end of its line, and a note is its
    /// text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Item::Func(x) => write!(f, "{x}"),
            Item::Decl(x) => writeln!(f, "{x}"),
            Item::Global(x) => writeln!(f, "{x}"),
            Item::Note(x) => f.write_str(x),
        }
    }
}

/// A module, or a unit of one: its items in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Module {
    pub items: Vec<Item>,
}

impl Module {
    pub fn push(&mut self, i: Item) {
        self.items.push(i);
    }

    /// A note: text only LLVM reads.
    pub fn note(&mut self, text: impl Into<String>) {
        self.items.push(Item::Note(text.into()));
    }

    pub fn funcs(&self) -> impl Iterator<Item = &Func> {
        self.items.iter().filter_map(|i| match i {
            Item::Func(f) => Some(f),
            _ => None,
        })
    }
}

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for i in &self.items {
            write!(f, "{i}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_print_as_llvm_writes_them() {
        let cases: Vec<(Inst, &str)> = vec![
            (
                Inst::Bin {
                    dst: "%t1".into(),
                    op: BinOp::Add,
                    ty: Ty::I64,
                    a: "%a0".into(),
                    b: "1".into(),
                },
                "%t1 = add i64 %a0, 1",
            ),
            (
                Inst::ICmp {
                    dst: "%t2".into(),
                    cond: ICmp::Ult,
                    ty: Ty::I64,
                    a: "%t1".into(),
                    b: "%t3".into(),
                },
                "%t2 = icmp ult i64 %t1, %t3",
            ),
            (
                Inst::Load {
                    dst: "%t4".into(),
                    ty: Ty::I8,
                    ptr: "@meadow_preempt".into(),
                    volatile: true,
                },
                "%t4 = load volatile i8, ptr @meadow_preempt",
            ),
            (
                Inst::Store {
                    ty: Ty::I64,
                    v: "%t4".into(),
                    ptr: "%t5".into(),
                    volatile: false,
                },
                "store i64 %t4, ptr %t5",
            ),
            (
                Inst::Gep {
                    dst: "%t6".into(),
                    elem: Ty::I64,
                    base: "%t5".into(),
                    index: "2".into(),
                },
                "%t6 = getelementptr i64, ptr %t5, i64 2",
            ),
            (
                Inst::Phi {
                    dst: "%t7".into(),
                    ty: Ty::I64,
                    incoming: vec![("%a0".into(), "b1".into()), ("0".into(), "b2".into())],
                },
                "%t7 = phi i64 [ %a0, %b1 ], [ 0, %b2 ]",
            ),
            (
                Inst::Call {
                    dst: Some("%t8".into()),
                    tail: true,
                    cc: Some("ghccc".into()),
                    ret: Ty::I64,
                    callee: "@mw.L.f.1".into(),
                    args: vec![(Ty::I64, "%ctx".into()), (Ty::I64, "%t7".into())],
                },
                "%t8 = tail call ghccc i64 @mw.L.f.1(i64 %ctx, i64 %t7)",
            ),
            (
                Inst::Call {
                    dst: None,
                    tail: false,
                    cc: None,
                    ret: Ty::Void,
                    callee: "@meadow_preempted".into(),
                    args: vec![],
                },
                "call void @meadow_preempted()",
            ),
            (
                Inst::CondBr {
                    cond: "%t2".into(),
                    yes: "b1".into(),
                    no: "b2".into(),
                    unlikely: Some(1),
                },
                "br i1 %t2, label %b1, label %b2, !prof !1",
            ),
            (
                Inst::Switch {
                    ty: Ty::I64,
                    v: "%t9".into(),
                    default: "b3".into(),
                    cases: vec![(4, "b4".into()), (5, "b5".into())],
                },
                "switch i64 %t9, label %b3 [ i64 4, label %b4 i64 5, label %b5 ]",
            ),
        ];
        for (i, want) in cases {
            assert_eq!(i.to_string(), want);
        }
    }

    #[test]
    fn operands_read_back_as_they_are_written() {
        assert_eq!(Operand::parse("%t3"), Some(Operand::Reg("t3")));
        assert_eq!(Operand::parse("-7"), Some(Operand::Int(-7)));
        assert_eq!(Operand::parse("true"), Some(Operand::Int(1)));
        assert_eq!(Operand::parse("@mw.s.1"), Some(Operand::Global("mw.s.1")));
        assert_eq!(
            Operand::parse("0x3FF0000000000000"),
            Some(Operand::Float(1.0f64.to_bits()))
        );
        assert_eq!(Operand::parse("null"), Some(Operand::Null));
    }

    #[test]
    fn a_function_prints_as_llvm_writes_it() {
        let f = Func {
            name: "@mw.L.f.1".into(),
            linkage: "hidden".into(),
            cc: Some("ghccc".into()),
            ret: Some(Ty::I64),
            params: vec![(Ty::I64, "%a0".into())],
            attrs: "noinline".into(),
            unlabelled: false,
            entry: vec![],
            allocas: vec![Inst::Alloca {
                dst: "%t1".into(),
                words: 2,
            }],
            body: vec![
                Inst::Br("b1".into()),
                Inst::Label("b1".into()),
                Inst::Ret {
                    ty: Ty::I64,
                    v: Some("%a0".into()),
                },
            ],
        };
        assert_eq!(
            f.to_string(),
            "define hidden ghccc i64 @mw.L.f.1(i64 %a0) noinline {\nentry:\n  %t1 = alloca [2 x i64]\n  br label %b1\nb1:\n  ret i64 %a0\n}\n\n"
        );
    }
}
