//! **Native code, from bytecode.**
//!
//! A baseline compiler: each block of bytecode (see [`crate::abi`]) becomes one
//! native function, instruction by instruction. What a word-level instruction
//! does -- a move, a constant, typed arithmetic, a compare-and-branch -- is
//! done inline, on the register file in memory. Everything else is handed back
//! to the interpreter one instruction at a time, through
//! [`crate::abi::meadow_exec`], and native code carries on or returns according
//! to what became of control.
//!
//! So the machine code is a translation of the typed bytecode, not a second
//! compiler: every engine runs the same instructions, and the difference is
//! only which of them an engine does itself.
//!
//! Two architectures, [`Arch::Aarch64`] and [`Arch::X86_64`], behind one small
//! [`Emit`] interface, and the same translation for both. The code needs no
//! relocations: it reaches the machine through fixed offsets in [`crate::Vm`]
//! -- see [`layout`] -- branches only within a function, and calls the
//! interpreter through a pointer the machine holds. That is what lets one
//! translation serve an object file on disk ([`object`]) and memory made
//! executable in place alike.
//!
//! # What native code keeps up
//!
//! The machine state an instruction changes: the register it writes, `live`
//! when that raises it, the pc when control leaves, and `steps`, counted in a
//! machine register and added to the machine's before every call and return.
//!
//! # Optimization
//!
//! The same [`OptLevel`] as the rest of the compiler, and the same promise:
//! each level runs the same instructions to the same effect, and differs only
//! in what it costs.
//!
//! * [`OptLevel::O0`] is the translation done literally, instruction by
//!   instruction: what a miscompilation is bisected against.
//! * [`OptLevel::O1`] keeps the machine's books where they are *observed*
//!   rather than where they change -- see [`Book`] -- and picks the short
//!   encodings of instructions with a constant operand. A run of arithmetic
//!   between two calls into the interpreter costs one addition to the step
//!   counter and at most one write to `live`, where it cost one of each per
//!   instruction. And it *chains* -- see below.
//! * [`OptLevel::O2`] adds two passes that trade code size for speed. A
//!   block's function takes in the code that loops back into it (see
//!   [`region`]), so a loop whose body the bytecode lays out as blocks of their
//!   own runs round natively instead of returning to the machine at every
//!   block. And the registers that function uses most live in machine
//!   registers (see [`pins`]), in memory only where something other than the
//!   function could look.
//!
//! # Chaining
//!
//! Every transfer of control in the bytecode is a tail call -- a jump to a
//! block with its arguments in the registers, or an invoke of a closure,
//! which is also how a function returns -- so nothing is left on the native
//! stack when control leaves a block. Without chaining, native code returns to
//! [`crate::Vm::advance`] there, which looks up the next block's function and
//! calls it: a return, a trip round the scheduler's loop and a prologue for
//! every call and every return in the program.
//!
//! Every function builds the same frame, so from [`OptLevel::O1`] one goes
//! straight on to the next instead ([`Emit::chain`]): it writes its pinned
//! registers back and jumps to the other function's *warm entry*, [`Emit::WARM`]
//! bytes in, past the frame. The frame, the uncounted steps and the loop count
//! carry on. In a whole program's code the target is a known label; for a
//! function compiled on its own, and for an invoke's target, it is found in the
//! machine's table of native functions ([`layout::NATIVE_TABLE`]) -- and where
//! that has nothing yet, the function returns as before, and the JIT counts
//! the entry. [`CHAINS`] such jumps in one call from the machine, and it
//! returns anyway, so the scheduler still gets its turn.

pub mod a64;
pub mod object;
pub mod thin;
pub mod vector;
pub mod x64;

use crate::value::Value;
use meadow_bytecode::{Cond, Const, Instr, Op, Pc, Program, Reg};
use meadow_rt::OptLevel;
use std::collections::HashMap;

/// Where the fields of [`crate::Vm`] native code touches are.
pub mod layout {
    /// `*mut u64`: the register file.
    pub const REGS: u32 = 0;
    pub const LIVE: u32 = 8;
    pub const PC: u32 = 16;
    pub const AT: u32 = 24;
    pub const STEPS: u32 = 32;
    /// `extern "C" fn(vm, pc) -> status`.
    pub const EXEC: u32 = 40;
    /// `*const u32`: every method table's pcs, in a row.
    pub const METHOD_PCS: u32 = 48;
    /// `*const u32`: where each table starts among them, and the last ends.
    pub const METHOD_STARTS: u32 = 56;
    /// `*const *const u8`: the native function for each pc, or null.
    pub const NATIVE_TABLE: u32 = 64;
    /// `*const *const u8`: the method entry for each pc, or null.
    pub const NATIVE_METHODS: u32 = 72;
    /// The heap's nursery, as `crate::heap::Heap` begins.
    pub const BASE: u32 = 80;
    pub const CAP: u32 = 88;
    pub const TOP: u32 = 96;
    pub const ALLOCATED: u32 = 104;
    pub const REGION_GROWTH: u32 = 112;
    /// `[*const *mut Word; 2]`: the block table of each generation, indexed by
    /// `addr >> 30`. See [`crate::heap::Heap::tables`] and [`super::thin`].
    pub const TABLES: u32 = 120;
    /// The frame stack's top and the end of its current chunk, in slots. Both
    /// are 32-bit heap addresses, so they sit four bytes apart. See
    /// `Heap::push_frame`.
    pub const FSP: u32 = 136;
    pub const FLIM: u32 = 140;
    /// The current chunk's base address (32-bit) and its first slot as a
    /// machine address: a frame in the current chunk is one load away.
    pub const FCUR: u32 = 144;
    pub const FBASE: u32 = 152;
    /// `fbase - fcur * 8`, a machine address: a frame `a` in the current
    /// chunk is at `fbias + a * 8`.
    pub const FBIAS: u32 = 160;
}

/// Where a heap address keeps each part, for the two-level lookup native code
/// reads a heap word with. See [`crate::heap::Heap::tables`].
pub mod addr {
    /// Which generation: 0 for the nursery, 1 for the old generation. A
    /// compact region is above both and is guarded out before this is read.
    pub const GEN_SHIFT: u32 = 30;
    /// Which block within it.
    pub const BLOCK_SHIFT: u32 = 13;
    pub const BLOCK_MASK: u64 = (1 << 17) - 1;
    /// Which slot within that.
    pub const SLOT_MASK: u64 = (1 << 13) - 1;
}

/// A target instruction set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
}

impl Arch {
    /// The architecture this process is running on, if it is one of these.
    pub fn host() -> Option<Arch> {
        if cfg!(target_arch = "aarch64") {
            Some(Arch::Aarch64)
        } else if cfg!(target_arch = "x86_64") {
            Some(Arch::X86_64)
        } else {
            None
        }
    }
}

/// A program's native code: the machine code, and where in it the function for
/// each block starts.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub arch: Arch,
    pub code: Vec<u8>,
    /// `(entry pc, offset into code)`, by pc.
    pub blocks: Vec<(Pc, u32)>,
    /// `(method pc, offset into code)` of every method entry: see
    /// [`Emit::stub`].
    pub stubs: Vec<(Pc, u32)>,
}

/// A position in the code being emitted, bound once and jumped to any number of
/// times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label(pub usize);

/// An operand that is a register or a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    Reg(Reg),
    Imm(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    /// Arithmetic: the sign is carried in, as `i64::wrapping_shr` does.
    Shr,
    /// Logical: zeros come in.
    Ushr,
    And,
}

/// A one-operand typed instruction. See [`Emit::unary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// The number of set bits, as an `Int`.
    PopCount,
    /// An `Int` as the nearest `Float`.
    ToFloat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// What one architecture's code for the translation looks like. Every method
/// emits code at the end of what is emitted so far; "`r`" is a register of the
/// bytecode machine. The first [`Emit::FIXED`] of them live in machine
/// registers, the same ones in every function, so control passes from one
/// native function to another with nothing moved; the architecture's code
/// writes them to memory where the interpreter could look and loads them back
/// where it enters from it. The rest live in memory.
pub trait Emit {
    /// How many bytecode registers live in machine registers.
    const FIXED: usize;
    /// How far into every function its warm entry is: past the part of the
    /// prologue that makes the frame, which a chained function shares with
    /// the one it came from.
    const WARM: usize;

    fn new() -> Self
    where
        Self: Sized;
    /// Where the next byte goes.
    fn offset(&self) -> usize;
    fn label(&mut self) -> Label;
    fn bind(&mut self, l: Label);
    /// How the next function is emitted: at `opt`, with `floats` -- fixed
    /// registers the function only ever does float arithmetic on -- kept in
    /// floating-point registers for its duration. Before [`Emit::prologue`].
    fn configure(&mut self, opt: OptLevel, floats: &[Reg]);
    /// A function's entry: save what it uses, and find the register file. The
    /// warm entry is [`Emit::WARM`] in, and `warm`, if given, is bound there.
    fn prologue(&mut self, warm: Option<Label>);
    /// Return `status`, having counted the steps taken.
    fn ret(&mut self, status: u32);
    /// Leave for `pc`: set the machine's pc -- and, for a jump, how many
    /// registers are live there -- then return [`crate::abi::JUMPED`].
    fn leave(&mut self, pc: Pc, live: Option<u32>);
    /// Go on to the native function for `pc`, at its warm entry: straight to
    /// `to` if that is given -- it is bound to the function in this same code
    /// -- or through the machine's table. Leave for `pc` instead if it has no
    /// function yet, or this call has made [`CHAINS`] such jumps.
    fn chain(&mut self, pc: Pc, live: Option<u32>, to: Option<Label>);
    /// One instruction retired.
    fn step(&mut self);
    /// One fewer: the instruction is handed to the interpreter after all, and
    /// the interpreter counts it.
    fn unstep(&mut self);
    /// `n` instructions retired at once.
    fn add_steps(&mut self, n: u32);
    fn jump(&mut self, to: Label);
    /// `r[a] = r[b]`
    fn mov(&mut self, a: Reg, b: Reg);
    /// `r[a] = w`
    fn word(&mut self, a: Reg, w: u64);
    /// `r[a] = r[b] op c`, as `Int`s. `Div` and `Rem` with a zero divisor jump
    /// to `zero` instead.
    fn int(&mut self, op: IntOp, a: Reg, b: Reg, c: Operand, zero: Label);
    fn float(&mut self, op: FloatOp, a: Reg, b: Reg, c: Reg);
    /// `r[a] = op r[b]`.
    fn unary(&mut self, op: UnaryOp, a: Reg, b: Reg);
    /// `r[a] = ` a frame with header `header` and captures the registers `fields`,
    /// pushed on the frame stack -- or `slow` if its chunk is full, or there
    /// is none yet. The frame's address is its slot on the stack, an old
    /// address, so the captures are written through the block table.
    fn frame(&mut self, a: Reg, header: &[u64], fields: &[Reg], slow: Label);
    /// `r[a] = cond(r[b], c)`, as words.
    fn cmp_int(&mut self, cond: Cond, a: Reg, b: Reg, c: Operand);
    fn cmp_float(&mut self, cond: Cond, a: Reg, b: Reg, c: Reg);
    /// `if not cond(r[x], y) goto to`
    fn branch_int(&mut self, cond: Cond, x: Reg, y: Operand, to: Label);
    fn branch_float(&mut self, cond: Cond, x: Reg, y: Reg, to: Label);
    /// `if r[x] == 0 goto to`
    fn branch_zero(&mut self, x: Reg, to: Label);
    /// `live = n`
    fn set_live(&mut self, n: u32);
    /// `live = max(live, n)`: what writing a register does to `live`, unless
    /// the function was configured above [`OptLevel::O0`], where [`Book`] does
    /// this instead.
    fn raise_live_to(&mut self, n: u32);
    /// A loop's jump back: to `to`, unless this call of the function has taken
    /// [`BACK_EDGES`] of them, in which case to `over`.
    fn back_edge(&mut self, to: Label, over: Label);

    // The heap, where the common case is simple enough to do here: an object
    // in the nursery, with a header of two words (see `crate::object`).
    // Anything else goes to `slow`, where the interpreter does it.

    /// Fall through if `r[x]` is data tagged `tag`; `miss` if it is some other
    /// nursery object.
    fn tag_test(&mut self, x: Reg, tag: u32, miss: Label, slow: Label);
    /// Carry out a run of [`thin::Step`]s, jumping to `slow` at the first
    /// guard that does not hold -- having written nothing, so the interpreter
    /// can be asked for the instruction instead.
    ///
    /// This is the *only* method an architecture has to write for the thin
    /// layer, and it is what keeps adding an expansion from costing any
    /// assembly: a new one is a new run of the same seven kinds of step.
    fn steps(&mut self, run: &[thin::Step], slow: Label);
    /// `r[a] = ` field `i` of the data or array in `r[b]`.
    fn field(&mut self, a: Reg, b: Reg, i: u32, slow: Label);
    /// Whether [`Emit::tag_test`] and [`Emit::field`] leave the object they
    /// found where the next [`Emit::field`] of it can use it without finding
    /// it again -- see [`Emit::field_found`] -- across nothing but what
    /// [`Book`] emits between two instructions. An architecture that answers
    /// `false` has every field found afresh.
    const KEEPS_FOUND: bool = false;
    /// Find the object in `r[x]` as [`Emit::tag_test`] and [`Emit::field`]
    /// do, or on to `slow` if native code cannot read it.
    fn find(&mut self, _x: Reg, _slow: Label) {
        unreachable!("only where `KEEPS_FOUND`")
    }
    /// [`Emit::field`] of the object the instruction before found -- or
    /// [`Emit::find`] did.
    fn field_found(&mut self, _a: Reg, _i: u32, _slow: Label) {
        unreachable!("only where `KEEPS_FOUND`")
    }
    /// Enter method `method` of the closure in `r[obj]`, with the `argc`
    /// arguments at `r[base]`, as `Op::Invoke` does -- and go on to it, as
    /// [`Emit::chain`] does from [`OptLevel::O1`], or return, leaving for it.
    /// Where the method's pc has an entry ([`Emit::stub`]), through that,
    /// with the arguments moved to `r[0..argc]`; otherwise by rebuilding the
    /// register file in memory as the interpreter does.
    fn invoke(&mut self, obj: Reg, method: u8, base: Reg, argc: u32, slow: Label);
    /// [`Emit::invoke`] for a call a profile says enters `guess.known`: guard
    /// that it does, and if so rebuild the register file as that target
    /// wants it -- captures and arguments whose number is known here -- and
    /// go on to it as [`Emit::chain`] does. If the guard fails, on to
    /// `generic`, where the generic invoke is. Answers whether it emitted
    /// anything; an architecture without it answers `false`, having emitted
    /// nothing, and every call is generic.
    fn invoke_known(
        &mut self,
        _obj: Reg,
        _base: Reg,
        _argc: u32,
        _guess: Guess,
        _generic: Label,
    ) -> bool {
        false
    }
    /// The function's *method entry*, for a function that begins a method
    /// whose object holds `captures` values and whose block takes `params`
    /// registers, captures included. Entered from [`Emit::invoke`] with the
    /// object's first word's address in the scratch register the fast paths
    /// keep it in, and the arguments in `r[0..params - captures]`: it moves
    /// the arguments up past the captures, loads the captures from the
    /// object, sets `live`, and goes on to `warm`, the function's warm entry.
    /// Answers whether it emitted one; an architecture without them answers
    /// `false`, and every call takes the other path.
    fn stub(&mut self, captures: u32, params: u32, warm: Label) -> bool;
    /// `r[a] = ` a new object whose header is the words `header` and whose
    /// fields are the registers `fields`, bumped into the nursery if it
    /// has room.
    fn alloc(&mut self, a: Reg, header: &[u64], fields: &[Reg], slow: Label);
    /// Have the interpreter carry out the instruction at `pc`, and return what
    /// it says unless that is [`crate::abi::CONTINUE`].
    fn exec(&mut self, pc: Pc);
    /// A loop's preheader and vector version -- see [`vector`] -- placed
    /// just before the loop's header, which everything falls into: the rest
    /// of the iterations, or all of them if a check fails, which is a jump
    /// to `scalar`. An architecture with no vector unit to speak of emits
    /// nothing.
    fn vector_loop(&mut self, plan: &vector::Plan, scalar: Label);
    /// The function is done: emit what its code shares.
    fn end(&mut self);
    /// Whether every branch emitted so far reaches its target -- which, on an
    /// architecture whose direct branches reach less than the whole of a
    /// large program's code, a jump from one block's function to another's
    /// may not.
    fn reaches(&self) -> bool {
        true
    }
    /// The machine code, with every branch resolved.
    fn finish(self) -> Vec<u8>;
}

/// How many times a loop inside one native function goes round before the
/// function returns anyway: the scheduler gets its turn between calls, not
/// inside one.
pub const BACK_EDGES: u32 = 1 << 12;

/// How many jumps from one native function straight into the next one call
/// from the machine makes before it returns anyway, for the same reason as
/// [`BACK_EDGES`] -- counted with them. Fewer, since a chained function is
/// itself a slice's worth of work: the scheduler counts what it calls, and a
/// chain of these is one call.
pub const CHAINS: u32 = 1 << 8;

/// The one thing an `invoke` site was seen to enter, by a profile: a frame
/// returning to pc `meta`, or a closure made with method table `meta`.
///
/// The types say what kind of thing a call enters, never which: a function
/// value is whatever was passed in, a return goes wherever the caller said.
/// Most sites only ever see one -- three quarters of the calls through
/// closures in parsing code -- and for those the generic `invoke`, which
/// finds out everything at run time, can be one guard and code written for
/// the answer. See [`Emit::invoke_known`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Known {
    pub frame: bool,
    pub meta: u32,
}

/// What a profile saw each `invoke` site enter, for the sites that only ever
/// entered one thing, by pc.
pub type Calls = HashMap<Pc, Known>;

/// What [`Emit::invoke_known`] is told: the guard, and everything that
/// follows from it holding.
#[derive(Debug, Clone, Copy)]
pub struct Guess {
    pub known: Known,
    /// The method the call goes to.
    pub target: Pc,
    /// How many captures the object holds, and so how long its header is.
    pub captures: u32,
    /// The target's function, when it is in the same code as this one.
    pub to: Option<Label>,
}

/// The [`Guess`] for the `invoke` `i` at `pc`, if the profile has one that
/// makes sense of the program: a method the object has, taking the arguments
/// the call passes.
fn guess(
    program: &Program,
    i: Instr,
    known: Known,
    shapes: &HashMap<Pc, (u8, u8)>,
) -> Option<(Pc, u32)> {
    let target = if known.frame {
        known.meta
    } else {
        *program
            .methods
            .get(known.meta as usize)?
            .get(i.b as usize)?
    };
    let (captures, params) = *shapes.get(&target)?;
    if known.frame && i.b != 0 {
        return None;
    }
    if !known.frame && program.method_captures.get(known.meta as usize) != Some(&captures) {
        return None;
    }
    (captures as u32 + i.imm == params as u32).then_some((target, captures as u32))
}

/// Compile every block of `program` for `arch`, at `opt`.
pub fn compile(program: &Program, arch: Arch, opt: OptLevel) -> Compiled {
    compile_guided(program, arch, opt, &Calls::new())
}

/// [`compile`], with what a profile saw each call site reach: see [`Known`].
pub fn compile_guided(program: &Program, arch: Arch, opt: OptLevel, calls: &Calls) -> Compiled {
    match arch {
        Arch::Aarch64 => compile_with::<a64::Asm>(program, arch, opt, calls),
        Arch::X86_64 => compile_with::<x64::Asm>(program, arch, opt, calls),
    }
}

/// What compiling one block at a time needs to know about the whole program:
/// worked out once, when the JIT starts, and not again for every block it
/// compiles -- each of these is a pass over every instruction, and a program
/// with the standard library in it has tens of thousands.
pub struct Whole {
    loops: HashMap<usize, u32>,
    preds: Preds,
    shapes: HashMap<Pc, (u8, u8)>,
}

impl Whole {
    pub fn of(program: &Program) -> Whole {
        Whole {
            loops: loop_live(program),
            preds: preds(program),
            shapes: method_shapes(program),
        }
    }
}

/// Compile the one block starting at `entry`, at `opt`: a function on its own,
/// for placing anywhere -- and where in it its method entry is, if it has
/// one.
pub fn compile_block(
    program: &Program,
    arch: Arch,
    entry: Pc,
    opt: OptLevel,
    whole: &Whole,
    calls: &dyn Fn(Pc) -> Option<Known>,
) -> (Vec<u8>, Option<u32>) {
    fn with<E: Emit>(
        program: &Program,
        entry: Pc,
        opt: OptLevel,
        whole: &Whole,
        calls: &dyn Fn(Pc) -> Option<Known>,
    ) -> (Vec<u8>, Option<u32>) {
        let mut asm = E::new();
        let shape = whole.shapes.get(&entry).copied();
        let stub = block(
            &mut asm,
            program,
            entry,
            opt,
            None,
            shape,
            &whole.loops,
            &whole.preds,
            &whole.shapes,
            calls,
        );
        (asm.finish(), stub)
    }
    match arch {
        Arch::Aarch64 => with::<a64::Asm>(program, entry, opt, whole, calls),
        Arch::X86_64 => with::<x64::Asm>(program, entry, opt, whole, calls),
    }
}

fn compile_with<E: Emit>(program: &Program, arch: Arch, opt: OptLevel, calls: &Calls) -> Compiled {
    let entries = crate::abi::block_entries(program);
    let shapes = method_shapes(program);
    let loops = loop_live(program);
    let preds = preds(program);
    // Every function jumps straight to the next one's code where it can. A
    // program big enough that such a jump cannot reach -- aarch64's `b` goes
    // 128 MiB either way -- is compiled again with none, each going through
    // the machine's table as a JIT-compiled function does.
    let whole = |linked: bool| {
        let mut asm = E::new();
        let mut blocks = Vec::with_capacity(entries.len());
        let mut stubs = Vec::new();
        let mut links = Links {
            entries: entries.iter().map(|&pc| pc as usize).collect(),
            warm: HashMap::new(),
        };
        for &entry in &entries {
            blocks.push((entry, asm.offset() as u32));
            let shape = shapes.get(&entry).copied();
            if let Some(at) = block(
                &mut asm,
                program,
                entry,
                opt,
                linked.then_some(&mut links),
                shape,
                &loops,
                &preds,
                &shapes,
                &|pc| calls.get(&pc).copied(),
            ) {
                stubs.push((entry, at));
            }
        }
        (asm, blocks, stubs)
    };
    let (mut asm, mut blocks, mut stubs) = whole(true);
    if !asm.reaches() {
        (asm, blocks, stubs) = whole(false);
    }
    Compiled {
        arch,
        code: asm.finish(),
        blocks,
        stubs,
    }
}

/// For every pc a loop goes back to -- the target of a `Jump` at or after it
/// -- how many registers the loop writes: one past the highest register any
/// instruction from the target to the jump writes, over every such loop.
///
/// What it is for: `live`, the high-water mark the collector reads, would
/// otherwise be raised again on every trip round, because the loop's header
/// is a join and a join forgets what `live` was. Every way into the header
/// brings `live` up to this instead -- a jump sets it, anything else raises
/// it -- and the header then knows it, and nothing in the loop raises it.
/// Raising it above what the compiler said is safe: the collector reads each
/// pc's map for which registers hold addresses, and `live` only bounds it.
///
/// A jump to an earlier pc is also what a call to a known function is, so
/// there are as many of these as calls, and each asks about a range: the
/// ranges are answered from a sparse table, built once per program, rather
/// than walked -- walked, the whole thing was quadratic, and the JIT asked
/// for it once per block it compiled.
pub fn loop_live(program: &Program) -> HashMap<usize, u32> {
    let n = program.code.len();
    if n == 0 {
        return HashMap::new();
    }
    // `table[k][i]`: the most registers written by any of the 2^k
    // instructions from `i`.
    let mut table: Vec<Vec<u32>> = vec![
        program
            .code
            .iter()
            .map(|i| writes(i).map_or(0, |r| r as u32 + 1))
            .collect(),
    ];
    let mut span = 1;
    while span * 2 <= n {
        let prev = table.last().expect("a first row");
        let row: Vec<u32> = (0..=n - span * 2)
            .map(|i| prev[i].max(prev[i + span]))
            .collect();
        table.push(row);
        span *= 2;
    }
    let most = |lo: usize, hi: usize| -> u32 {
        // Over `lo..=hi`: two spans of the largest power of two that fits,
        // overlapping in the middle, which a maximum does not mind.
        let len = hi - lo + 1;
        let k = usize::BITS - 1 - len.leading_zeros();
        let row = &table[k as usize];
        row[lo].max(row[hi + 1 - (1 << k)])
    };
    let mut out: HashMap<usize, u32> = HashMap::new();
    for (pc, i) in program.code.iter().enumerate() {
        if i.op != Op::Jump || i.imm as usize > pc {
            continue;
        }
        let top = most(i.imm as usize, pc);
        let e = out.entry(i.imm as usize).or_insert(0);
        *e = (*e).max(top);
    }
    out
}

/// Where control can arrive at each pc from elsewhere: by the pc, every
/// instruction that jumps or branches to it. What a vector loop's body must
/// have none of from outside itself -- and a pc a method table or a
/// definition names is arrived at from anywhere, so those are `anchored`.
pub struct Preds {
    pub from: HashMap<usize, Vec<usize>>,
    pub anchored: std::collections::HashSet<usize>,
}

pub fn preds(program: &Program) -> Preds {
    let mut from: HashMap<usize, Vec<usize>> = HashMap::new();
    for (pc, i) in program.code.iter().enumerate() {
        let target = match i.op {
            Op::Jump | Op::JumpUnless | Op::JumpUnlessTag | Op::BrI | Op::BrIK | Op::BrF => {
                Some(i.imm as usize)
            }
            _ => None,
        };
        if let Some(t) = target {
            from.entry(t).or_default().push(pc);
        }
    }
    let anchored = program
        .methods
        .iter()
        .flatten()
        .map(|&pc| pc as usize)
        .chain(program.entries.iter().map(|&pc| pc as usize))
        .chain(program.entry.map(|pc| pc as usize))
        .collect();
    Preds { from, anchored }
}

/// The register an instruction writes, if it writes one. Anything not known
/// to leave every register alone is taken to write `a`, which can only make
/// [`loop_live`] larger.
fn writes(i: &Instr) -> Option<Reg> {
    match i.op {
        Op::Nop
        | Op::Invoke
        | Op::Jump
        | Op::JumpUnless
        | Op::JumpUnlessTag
        | Op::BrI
        | Op::BrIK
        | Op::BrF
        | Op::Halt
        | Op::Error => None,
        _ => Some(i.a),
    }
}

/// The shape of the method beginning at each pc that begins one: how many
/// captures its object holds, and how many registers its block takes. A pc
/// two tables disagree about -- which the compiler does not produce -- has no
/// shape, and every call to it takes the general path.
fn method_shapes(program: &Program) -> HashMap<Pc, (u8, u8)> {
    let mut out: HashMap<Pc, Option<(u8, u8)>> = HashMap::new();
    for (t, table) in program.methods.iter().enumerate() {
        let Some(&captures) = program.method_captures.get(t) else {
            continue;
        };
        for (k, &pc) in table.iter().enumerate() {
            let Some(&params) = program.method_params.get(t).and_then(|p| p.get(k)) else {
                continue;
            };
            let shape = (captures, params);
            out.entry(pc)
                .and_modify(|s| {
                    if *s != Some(shape) {
                        *s = None;
                    }
                })
                .or_insert(Some(shape));
        }
    }
    out.into_iter()
        .filter_map(|(pc, s)| s.map(|s| (pc, s)))
        .collect()
}

/// Does control never fall through from `i` to the instruction after it?
fn ends(i: &Instr) -> bool {
    matches!(i.op, Op::Jump | Op::Invoke | Op::Halt | Op::Error)
}

/// Where native code can go from the instruction at `pc`, without returning:
/// the next instruction, and a branch's target -- not the targets of the
/// instructions the interpreter does, which return when they branch.
fn successors(code: &[Instr], pc: usize) -> impl Iterator<Item = usize> {
    let i = code[pc];
    let (next, target) = match i.op {
        Op::Jump => (None, Some(i.imm as usize)),
        Op::JumpUnless | Op::JumpUnlessTag | Op::BrI | Op::BrIK | Op::BrF => {
            (Some(pc + 1), Some(i.imm as usize))
        }
        Op::Invoke | Op::Halt | Op::Error => (None, None),
        _ => (Some(pc + 1), None),
    };
    let len = code.len();
    next.into_iter().chain(target).filter(move |&to| to < len)
}

/// How many instructions past its own block [`region`] looks for a loop in.
const REGION_BUDGET: usize = 512;

/// The instructions `entry`'s function holds, in the order it holds them:
/// the block, from `entry` up to and including the instruction that leaves --
/// through the entries of other blocks on the way, which are only where control
/// may also arrive from elsewhere.
///
/// At [`OptLevel::O2`], also every instruction within [`REGION_BUDGET`] that
/// control can go to from the block and come back into it from: the rest of
/// the loops the block is in, after it in pc order. What does not come back
/// stays out, and is left for, as from any block -- so what is repeated is
/// loops, not everything a function could reach.
fn region(code: &[Instr], entry: usize, opt: OptLevel) -> Vec<usize> {
    let mut end = entry;
    loop {
        let i = &code[end];
        end += 1;
        if ends(i) || end >= code.len() {
            break;
        }
    }
    let mut order: Vec<usize> = (entry..end).collect();
    if opt < OptLevel::O2 {
        return order;
    }

    // Forward: everything reachable, as far as the budget, and the edges into
    // each instruction found.
    let block = entry..end;
    let mut preds: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut work: Vec<usize> = order.clone();
    let mut seen: std::collections::HashSet<usize> = order.iter().copied().collect();
    while let Some(pc) = work.pop() {
        for to in successors(code, pc) {
            preds.entry(to).or_default().push(pc);
            if seen.len() - order.len() < REGION_BUDGET && seen.insert(to) {
                work.push(to);
            }
        }
    }
    // Backward from the block: what comes back into it.
    let mut back: std::collections::HashSet<usize> = order.iter().copied().collect();
    let mut work = order.clone();
    while let Some(pc) = work.pop() {
        for &from in preds.get(&pc).map_or(&[][..], Vec::as_slice) {
            if seen.contains(&from) && back.insert(from) {
                work.push(from);
            }
        }
    }
    let mut rest: Vec<usize> = back.into_iter().filter(|pc| !block.contains(pc)).collect();
    rest.sort_unstable();
    order.extend(rest);
    order
}

/// The fixed registers `entry`'s function only ever does float arithmetic on:
/// those it keeps in floating-point registers, so that the arithmetic needs no
/// move first. A register that is an `Int` somewhere in the function as well
/// -- which happens when the compiler reuses one -- stays in its general
/// register and moves for its float operations. Untyped uses -- moves,
/// captures, calls -- read and write either kind.
fn floats(code: &[Instr], f: &Function, fixed: usize) -> Vec<Reg> {
    let mut floats = [false; 256];
    let mut ints = [false; 256];
    for &pc in &f.order {
        let i = code[pc];
        let (int_operands, float_operands): (&[Reg], &[Reg]) = match i.op {
            Op::AddF | Op::SubF | Op::MulF | Op::DivF => (&[], &[i.a, i.b, i.c]),
            Op::CmpF => (&[i.a], &[i.b, i.c]),
            Op::BrF => (&[], &[i.a, i.b]),
            Op::ItoF => (&[i.b], &[i.a]),
            Op::AddI
            | Op::SubI
            | Op::MulI
            | Op::DivI
            | Op::ModI
            | Op::CmpI
            | Op::ShlI
            | Op::ShrI
            | Op::UshrI
            | Op::AndI => (&[i.a, i.b, i.c], &[]),
            Op::AddIK
            | Op::SubIK
            | Op::MulIK
            | Op::ShlIK
            | Op::ShrIK
            | Op::UshrIK
            | Op::AndIK
            | Op::CmpIK
            | Op::PopI
            | Op::BrI => (&[i.a, i.b], &[]),
            Op::BrIK | Op::JumpUnless | Op::JumpUnlessTag => (&[i.a], &[]),
            _ => (&[], &[]),
        };
        int_operands.iter().for_each(|&r| ints[r as usize] = true);
        float_operands
            .iter()
            .for_each(|&r| floats[r as usize] = true);
    }
    (0..fixed.min(256))
        .filter(|&r| floats[r] && !ints[r])
        .map(|r| r as Reg)
        .collect()
}

fn alloc_kind(op: Op) -> crate::heap::Kind {
    match op {
        Op::MakeData => crate::heap::Kind::Data,
        Op::Frame => crate::heap::Kind::Frame,
        Op::MakeArray => crate::heap::Kind::Array,
        _ => crate::heap::Kind::Closure,
    }
}

/// The machine bookkeeping a function owes but has not written yet, from
/// [`OptLevel::O1`].
///
/// Instructions retired and the register high-water mark accumulate here while
/// the code between two *observation points* is emitted, and are settled --
/// one [`Emit::add_steps`], at most one [`Emit::raise_live_to`] -- at the next
/// one. What can observe them:
///
/// * a call into the interpreter, which reads `live` to find the collector's
///   roots, and whose steps are counted after the ones before it;
/// * a return, which hands both to the machine;
/// * a branch, since the code at the other end runs as if both were current;
/// * an instruction control reaches from more than one place, for the same
///   reason from the other side.
///
/// Everything between -- moves, constants, typed arithmetic, comparisons --
/// changes neither what the machine can see nor where it can go, so owing the
/// bookkeeping across it is invisible.
///
/// `known` is what `live` is at least, statically, which lets a raise the
/// machine has already done be skipped: after a jump's `live = n`, writing a
/// register below `n` needs nothing. It is forgotten where control joins,
/// since the other path may have known less.
struct Book {
    on: bool,
    steps: u32,
    live: u32,
    known: u32,
}

impl Book {
    fn new(opt: OptLevel) -> Book {
        Book {
            on: opt >= OptLevel::O1,
            steps: 0,
            live: 0,
            known: 0,
        }
    }

    /// One instruction retired.
    fn step<E: Emit>(&mut self, asm: &mut E) {
        if self.on {
            self.steps += 1;
        } else {
            asm.step();
        }
    }

    /// Register `a` was written.
    fn wrote(&mut self, a: Reg) {
        self.live = self.live.max(a as u32 + 1);
    }

    /// Settle everything owed: before anything that can observe it.
    fn sync<E: Emit>(&mut self, asm: &mut E) {
        if !self.on {
            return;
        }
        if self.steps > 0 {
            asm.add_steps(self.steps);
            self.steps = 0;
        }
        if self.live > self.known {
            asm.raise_live_to(self.live);
            self.known = self.live;
        }
        self.live = 0;
    }

    /// Settle the steps, and forget the writes: before something that sets
    /// `live` outright, which makes raising it first pointless.
    fn sync_steps<E: Emit>(&mut self, asm: &mut E) {
        if self.on && self.steps > 0 {
            asm.add_steps(self.steps);
        }
        self.steps = 0;
        self.live = 0;
    }

    /// `live` was just set to `n`.
    fn set(&mut self, n: u32) {
        self.known = n;
    }

    /// Where another path joins: settle, and know only what every path
    /// brings -- `known`, which is what [`loop_live`] says at a loop's header
    /// and nothing anywhere else.
    fn join<E: Emit>(&mut self, asm: &mut E, known: u32) {
        self.sync(asm);
        self.known = known;
    }

    /// Falling straight on into the next instruction, which may head a loop:
    /// then settle and raise as [`Book::entering`] does; otherwise nothing.
    fn sync_steps_if_entering<E: Emit>(&mut self, asm: &mut E, at: Option<u32>) {
        if at.is_some() {
            self.sync(asm);
            self.entering(asm, at);
        }
    }

    /// About to go to `pc`: if it heads a loop that wants `live` at `at`
    /// least, and that is more than is known, raise it, and know it.
    fn entering<E: Emit>(&mut self, asm: &mut E, at: Option<u32>) {
        if let Some(ll) = at
            && self.known < ll
        {
            asm.raise_live_to(ll);
            self.known = ll;
        }
    }
}

/// Every block's warm entry in a whole program's code, for jumping straight to:
/// the functions are all in one piece of code, so a jump from one to another
/// needs no table.
struct Links {
    /// The pcs that have a function.
    entries: std::collections::HashSet<usize>,
    /// Each one's warm entry, made the first time it is wanted.
    warm: HashMap<usize, Label>,
}

impl Links {
    fn warm<E: Emit>(&mut self, asm: &mut E, pc: usize) -> Option<Label> {
        self.entries
            .contains(&pc)
            .then(|| *self.warm.entry(pc).or_insert_with(|| asm.label()))
    }
}

/// One function being compiled: its instructions in order, and where each is.
struct Function<'a> {
    order: Vec<usize>,
    /// Position in `order`, by pc.
    index: HashMap<usize, usize>,
    labels: Vec<Label>,
    /// Where control leaves for a pc, having set it: `(label, pc)`.
    exits: Vec<(Label, Pc)>,
    /// A branch's way to a pc it cannot jump straight to: `(label, from, pc)`,
    /// `from` being the branch's position.
    detours: Vec<(Label, usize, usize)>,
    /// See [`loop_live`].
    loops: &'a HashMap<usize, u32>,
    /// Loops with a vector version: by the header's position, the label of
    /// its preheader and the position of its jump back. Control from outside
    /// the loop enters through the preheader; the jump back does not.
    pre: HashMap<usize, (Label, usize)>,
    /// Go straight on to the next block's function, from [`OptLevel::O1`].
    chain: bool,
    /// How long the program is: a pc past the end has nowhere to chain to.
    len: usize,
    links: Option<&'a mut Links>,
}

impl Function<'_> {
    /// Leave the function for `pc`, having set `live` if it is given: on to
    /// its native function, where there is one and the function chains.
    fn depart<E: Emit>(&mut self, asm: &mut E, pc: usize, live: Option<u32>) {
        if self.chain && pc < self.len {
            let to = self.links.as_mut().and_then(|l| l.warm(asm, pc));
            asm.chain(pc as Pc, live, to);
        } else {
            asm.leave(pc as Pc, live);
        }
    }

    /// A label that leaves for `pc`.
    fn exit<E: Emit>(&mut self, asm: &mut E, pc: usize) -> Label {
        let l = asm.label();
        self.exits.push((l, pc as Pc));
        l
    }

    /// Go from the instruction at position `from` to the one at `pc`: a jump,
    /// if the function holds it further on; a counted jump back, if it holds
    /// it earlier -- so that every loop in the function goes through one --
    /// and otherwise, depart for it.
    fn goto<E: Emit>(&mut self, asm: &mut E, from: usize, pc: usize) {
        match self.index.get(&pc) {
            Some(&t) if t > from => asm.jump(self.way_in(t, from)),
            Some(&t) => {
                let over = self.exit(asm, pc);
                asm.back_edge(self.way_in(t, from), over);
            }
            None => self.depart(asm, pc, None),
        }
    }

    /// The label to reach position `t` by from position `from`: its
    /// preheader, if it heads a vector loop and `from` is outside that loop.
    fn way_in(&self, t: usize, from: usize) -> Label {
        match self.pre.get(&t) {
            Some(&(pre, s)) if !(t..=s).contains(&from) => pre,
            _ => self.labels[t],
        }
    }

    /// Where a branch at position `from` to `pc` goes: straight to the
    /// instruction further on, or to a detour that does what [`Function::goto`]
    /// does.
    fn target<E: Emit>(&mut self, asm: &mut E, from: usize, pc: usize) -> Label {
        match self.index.get(&pc) {
            Some(&t) if t > from => self.way_in(t, from),
            _ => {
                let l = asm.label();
                self.detours.push((l, from, pc));
                l
            }
        }
    }

    /// The positions control reaches from somewhere other than the position
    /// before: a branch's target, where a fast path's slow half comes back,
    /// and the first of a run that does not follow on from the one before.
    fn joins(&self, code: &[Instr]) -> Vec<bool> {
        let mut out = vec![false; self.order.len()];
        for (k, &pc) in self.order.iter().enumerate() {
            let i = code[pc];
            let mut mark = |to: usize| {
                if let Some(&t) = self.index.get(&to) {
                    out[t] = true;
                }
            };
            match i.op {
                Op::Jump | Op::JumpUnless | Op::BrI | Op::BrIK | Op::BrF => mark(i.imm as usize),
                Op::JumpUnlessTag => {
                    mark(i.imm as usize);
                    mark(pc + 1);
                }
                Op::Field | Op::MakeData | Op::MakeArray | Op::Closure | Op::Frame => mark(pc + 1),
                _ => {}
            }
            if !ends(&i) && self.order.get(k + 1) != Some(&(pc + 1)) {
                mark(pc + 1);
            }
            if k > 0 {
                let before = self.order[k - 1];
                if ends(&code[before]) || before + 1 != pc {
                    out[k] = true;
                }
            }
        }
        out
    }
}

/// One function: the instructions [`region`] gives `entry`, at `opt` -- with
/// `links` when every block is being compiled into the same code, and
/// `shape` if `entry` begins a method, for which the function gets a method
/// entry ([`Emit::stub`]): where that is, if it got one.
fn block<E: Emit>(
    asm: &mut E,
    program: &Program,
    entry: Pc,
    opt: OptLevel,
    mut links: Option<&mut Links>,
    shape: Option<(u8, u8)>,
    loops: &HashMap<usize, u32>,
    preds: &Preds,
    shapes: &HashMap<Pc, (u8, u8)>,
    calls: &dyn Fn(Pc) -> Option<Known>,
) -> Option<u32> {
    let code = &program.code;
    let order = region(code, entry as usize, opt);
    let index: HashMap<usize, usize> = order.iter().enumerate().map(|(k, &pc)| (pc, k)).collect();
    let labels: Vec<Label> = order.iter().map(|_| asm.label()).collect();
    // Loops with a vector version: a jump back to a header, over a body of
    // consecutive instructions none of which, past the header, is a block
    // entry -- since control arriving there from elsewhere would have
    // skipped the preheader.
    let mut plans: HashMap<usize, (vector::Plan, usize)> = HashMap::new();
    let mut pre: HashMap<usize, (Label, usize)> = HashMap::new();
    if opt >= OptLevel::O2 {
        for (s, &pc) in order.iter().enumerate() {
            let i = code[pc];
            if i.op != Op::Jump {
                continue;
            }
            let Some(&t) = index.get(&(i.imm as usize)) else {
                continue;
            };
            if t > s || plans.contains_key(&t) {
                continue;
            }
            let pcs = &order[t..=s];
            let consecutive = pcs.windows(2).all(|w| w[1] == w[0] + 1);
            let (lo, hi) = (pcs[0], pcs[pcs.len() - 1]);
            let sealed = pcs[1..].iter().all(|pc| {
                !preds.anchored.contains(pc)
                    && preds
                        .from
                        .get(pc)
                        .is_none_or(|srcs| srcs.iter().all(|s| (lo..=hi).contains(s)))
            });
            let tracing = std::env::var_os("MEADOW_VECTOR_DEBUG").is_some();
            if !consecutive || !sealed {
                if tracing {
                    eprintln!("vector: loop {lo}..={hi} consecutive={consecutive} sealed={sealed}");
                }
                continue;
            }
            let planned = vector::plan(program, pcs, E::FIXED);
            if tracing {
                eprintln!("vector: loop {lo}..={hi} plan={}", planned.is_some());
            }
            if let Some(p) = planned {
                plans.insert(t, (p, s));
                pre.insert(t, (asm.label(), s));
            }
        }
    }
    let warm = links
        .as_mut()
        .and_then(|l| l.warm(asm, entry as usize))
        .unwrap_or_else(|| asm.label());
    let mut f = Function {
        order,
        index,
        labels,
        exits: Vec::new(),
        detours: Vec::new(),
        loops,
        pre,
        chain: opt >= OptLevel::O1,
        len: code.len(),
        links,
    };
    let joins = f.joins(code);
    let floats = floats(code, &f, E::FIXED);
    // Fast paths' slow halves: where one starts, and the instruction's
    // position -- placed after the function's code, out of the way.
    let mut slows: Vec<(Label, usize)> = Vec::new();

    asm.configure(opt, &floats);
    asm.prologue(Some(warm));
    let mut book = Book::new(opt);
    // Entered at a loop's header, from the machine or another function: the
    // loop's `live` is owed here, since nothing before this could raise it.
    if let Some(&ll) = loops.get(&(entry as usize)) {
        asm.raise_live_to(ll);
        book.set(ll);
    }
    // The register whose object the instruction just emitted found and left
    // where the next can use it (`Emit::KEEPS_FOUND`): a tag test and the
    // field reads after it, `jumpunlesstag r0 is Fork; field r2 <- r0.0;
    // field r3 <- r0.1`, find `r0` once rather than three times -- which for
    // an old object is three dependent loads through the block tables each.
    let mut found: Option<Reg> = None;
    // Where a field that used the object found before it is entered from
    // anywhere but the instruction before -- its own label, which the slow
    // half of that instruction and every jump come to -- with what it takes
    // to find the object there: `(label, where the field's code is, register,
    // the field's slow half)`.
    let mut refinds: Vec<(Label, Label, Reg, Label)> = Vec::new();
    for k in 0..f.order.len() {
        let pc = f.order[k];
        let i = code[pc];
        // Only straight on from the instruction that found it, with nothing
        // but bookkeeping between; that instruction's slow half comes back
        // here, so this is a join, and the join is what `refinds` is for.
        let reuse = found.take().filter(|&x| {
            i.op == Op::Field
                && i.b == x
                && k > 0
                && f.order[k - 1] + 1 == pc
                && joins[k]
                && !plans.contains_key(&k)
        });
        // The preheader and vector version of a loop headed here: everything
        // from outside comes through it, and falls into the header after.
        if let Some((plan, _)) = plans.get(&k) {
            book.sync(asm);
            book.entering(asm, loops.get(&pc).copied());
            asm.bind(f.pre[&k].0);
            asm.vector_loop(plan, f.labels[k]);
        }
        if joins[k] {
            book.join(asm, loops.get(&pc).copied().unwrap_or(0));
        }
        if reuse.is_none() {
            asm.bind(f.labels[k]);
        }
        let pc32 = pc as Pc;
        match i.op {
            Op::Nop => book.step(asm),
            // A register moved to itself is already where it is going.
            Op::Move if book.on && i.a == i.b => book.step(asm),
            Op::Move => {
                book.step(asm);
                asm.mov(i.a, i.b);
                book.wrote(i.a);
            }
            Op::Const => match immediate(program, i.imm) {
                Some(w) => {
                    book.step(asm);
                    asm.word(i.a, w);
                    book.wrote(i.a);
                }
                None => {
                    book.sync(asm);
                    asm.exec(pc32);
                }
            },
            Op::Jump => {
                book.step(asm);
                book.sync_steps(asm);
                let to = i.imm as usize;
                // Into a loop, `live` for the whole loop: see `loop_live`.
                let live = (i.a as u32).max(loops.get(&to).copied().unwrap_or(0));
                if f.index.contains_key(&to) {
                    asm.set_live(live);
                    book.set(live);
                    f.goto(asm, k, to);
                } else {
                    f.depart(asm, to, Some(live));
                }
            }
            Op::JumpUnless => {
                book.step(asm);
                book.sync(asm);
                book.entering(asm, loops.get(&(i.imm as usize)).copied());
                let to = f.target(asm, k, i.imm as usize);
                asm.branch_zero(i.a, to);
            }
            Op::AddI
            | Op::SubI
            | Op::MulI
            | Op::DivI
            | Op::ModI
            | Op::ShlI
            | Op::ShrI
            | Op::UshrI
            | Op::AndI => {
                let op = match i.op {
                    Op::AddI => IntOp::Add,
                    Op::SubI => IntOp::Sub,
                    Op::MulI => IntOp::Mul,
                    Op::DivI => IntOp::Div,
                    Op::ShlI => IntOp::Shl,
                    Op::ShrI => IntOp::Shr,
                    Op::UshrI => IntOp::Ushr,
                    Op::AndI => IntOp::And,
                    _ => IntOp::Rem,
                };
                typed_int(asm, &mut book, op, i, Operand::Reg(i.c), pc32);
            }
            Op::AddIK | Op::SubIK | Op::MulIK | Op::ShlIK | Op::ShrIK | Op::UshrIK | Op::AndIK => {
                let op = match i.op {
                    Op::AddIK => IntOp::Add,
                    Op::SubIK => IntOp::Sub,
                    Op::ShlIK => IntOp::Shl,
                    Op::ShrIK => IntOp::Shr,
                    Op::UshrIK => IntOp::Ushr,
                    Op::AndIK => IntOp::And,
                    _ => IntOp::Mul,
                };
                let c = Operand::Imm(i.imm as i32 as i64);
                typed_int(asm, &mut book, op, i, c, pc32);
            }
            Op::PopI | Op::ItoF => {
                let op = if i.op == Op::PopI {
                    UnaryOp::PopCount
                } else {
                    UnaryOp::ToFloat
                };
                book.step(asm);
                asm.unary(op, i.a, i.b);
                book.wrote(i.a);
            }
            Op::AddF | Op::SubF | Op::MulF | Op::DivF => {
                let op = match i.op {
                    Op::AddF => FloatOp::Add,
                    Op::SubF => FloatOp::Sub,
                    Op::MulF => FloatOp::Mul,
                    _ => FloatOp::Div,
                };
                book.step(asm);
                asm.float(op, i.a, i.b, i.c);
                book.wrote(i.a);
            }
            Op::CmpI | Op::CmpIK | Op::CmpF | Op::BrI | Op::BrIK | Op::BrF => {
                let cond_byte = match i.op {
                    Op::CmpI | Op::CmpF => i.imm,
                    _ => i.c as u32,
                };
                let Some(cond) = Cond::from_byte(cond_byte) else {
                    // A malformed instruction: the interpreter says so.
                    book.sync(asm);
                    asm.exec(pc32);
                    fall_through(asm, &mut book, &mut f, code, k);
                    continue;
                };
                book.step(asm);
                match i.op {
                    Op::CmpI => asm.cmp_int(cond, i.a, i.b, Operand::Reg(i.c)),
                    Op::CmpIK => asm.cmp_int(cond, i.a, i.b, Operand::Imm(i.imm as i32 as i64)),
                    Op::CmpF => asm.cmp_float(cond, i.a, i.b, i.c),
                    Op::BrI => {
                        book.sync(asm);
                        book.entering(asm, loops.get(&(i.imm as usize)).copied());
                        let to = f.target(asm, k, i.imm as usize);
                        asm.branch_int(cond, i.a, Operand::Reg(i.b), to);
                    }
                    Op::BrIK => {
                        book.sync(asm);
                        book.entering(asm, loops.get(&(i.imm as usize)).copied());
                        let to = f.target(asm, k, i.imm as usize);
                        asm.branch_int(cond, i.a, Operand::Imm(i.b as i8 as i64), to);
                    }
                    _ => {
                        book.sync(asm);
                        book.entering(asm, loops.get(&(i.imm as usize)).copied());
                        let to = f.target(asm, k, i.imm as usize);
                        asm.branch_float(cond, i.a, i.b, to);
                    }
                }
                if matches!(i.op, Op::CmpI | Op::CmpIK | Op::CmpF) {
                    book.wrote(i.a);
                }
            }
            Op::JumpUnlessTag => {
                let slow = asm.label();
                book.step(asm);
                book.sync(asm);
                book.entering(asm, loops.get(&(i.imm as usize)).copied());
                let miss = f.target(asm, k, i.imm as usize);
                asm.tag_test(i.a, i.bc() as u32, miss, slow);
                slows.push((slow, k));
                if E::KEEPS_FOUND && book.on {
                    found = Some(i.a);
                }
            }
            Op::Field => {
                let slow = asm.label();
                // A step owed here and nothing else, the join having settled
                // the rest: what `refinds` does on the way in, before finding.
                debug_assert!(reuse.is_none() || book.steps == 0);
                book.step(asm);
                book.sync(asm);
                match reuse {
                    Some(x) => {
                        let here = asm.label();
                        asm.bind(here);
                        refinds.push((f.labels[k], here, x, slow));
                        asm.field_found(i.a, i.imm, slow);
                    }
                    None => asm.field(i.a, i.b, i.imm, slow),
                }
                book.wrote(i.a);
                slows.push((slow, k));
                if E::KEEPS_FOUND && book.on && i.a != i.b {
                    found = Some(i.b);
                }
            }
            Op::Invoke => {
                let slow = asm.label();
                book.step(asm);
                book.sync(asm);
                // Guarded on what a profile saw here, where it saw one thing,
                // and chaining only: the guess is worth nothing to a function
                // that returns to the machine at every block anyway.
                if f.chain
                    && let Some(known) = calls(pc32)
                    && let Some((target, captures)) = guess(program, i, known, shapes)
                {
                    let to = f.links.as_mut().and_then(|l| l.warm(asm, target as usize));
                    let generic = asm.label();
                    let g = Guess {
                        known,
                        target,
                        captures,
                        to,
                    };
                    asm.invoke_known(i.a, i.c, i.imm, g, generic);
                    asm.bind(generic);
                }
                asm.invoke(i.a, i.b, i.c, i.imm, slow);
                slows.push((slow, k));
            }
            Op::MakeData | Op::MakeArray | Op::Closure | Op::Frame => {
                let kind = alloc_kind(i.op);
                let meta = match kind {
                    crate::heap::Kind::Array => 0,
                    // A frame's `meta` is its return pc -- see `Vm::frame`.
                    crate::heap::Kind::Frame => program
                        .methods
                        .get(i.imm as usize)
                        .and_then(|t| t.first())
                        .copied()
                        .unwrap_or(0),
                    _ => i.imm,
                };
                match header(program, pc, kind, meta, i.c as usize) {
                    Some(h) => {
                        let slow = asm.label();
                        book.step(asm);
                        book.sync(asm);
                        let fields = program.fields(pc, i).regs();
                        if i.op == Op::Frame {
                            asm.frame(i.a, &h, &fields, slow);
                        } else {
                            asm.alloc(i.a, &h, &fields, slow);
                        }
                        book.wrote(i.a);
                        slows.push((slow, k));
                    }
                    None => {
                        book.sync(asm);
                        asm.exec(pc32);
                    }
                }
            }
            // Everything else that touches the heap, the tables or the
            // scheduler. A few of these have an expansion into thin steps and
            // are done here after all; the rest go to the interpreter, as they
            // always have. See [`thin`].
            _ => match thin::expand(program, pc, i) {
                Some(run) => {
                    let slow = asm.label();
                    book.step(asm);
                    book.sync(asm);
                    asm.steps(&run, slow);
                    book.wrote(i.a);
                    slows.push((slow, k));
                }
                None => {
                    book.sync(asm);
                    asm.exec(pc32);
                }
            },
        }
        fall_through(asm, &mut book, &mut f, code, k);
    }
    for (entry, here, x, slow) in refinds {
        asm.bind(entry);
        asm.add_steps(1);
        asm.find(x, slow);
        asm.jump(here);
    }
    for (l, k) in slows {
        let pc = f.order[k];
        asm.bind(l);
        asm.unstep();
        asm.exec(pc as Pc);
        if ends(&code[pc]) {
            // Handed to the interpreter, which never says to carry on after one
            // of these; should it, the machine is where it said.
            asm.ret(crate::abi::JUMPED);
        } else {
            // Back in from the cold, knowing nothing about `live`.
            Book::new(opt).entering(asm, loops.get(&(pc + 1)).copied());
            f.goto(asm, k, pc + 1);
        }
    }
    let mut d = 0;
    while d < f.detours.len() {
        let (l, from, pc) = f.detours[d];
        asm.bind(l);
        f.goto(asm, from, pc);
        d += 1;
    }
    for (l, pc) in std::mem::take(&mut f.exits) {
        asm.bind(l);
        asm.leave(pc, None);
    }
    asm.end();
    // The method entry, for a method whose captures and arguments all fit the
    // fixed registers: one whose block takes more lives in memory past them,
    // and is entered the general way.
    let (captures, params) = shape?;
    if params < captures || params as usize > E::FIXED {
        return None;
    }
    let at = asm.offset() as u32;
    asm.stub(captures as u32, params as u32, warm).then_some(at)
}

/// After the instruction at position `k`: on to the next one, if it is not the
/// one emitted next -- or return, after an instruction that never falls
/// through.
fn fall_through<E: Emit>(asm: &mut E, book: &mut Book, f: &mut Function, code: &[Instr], k: usize) {
    let pc = f.order[k];
    let i = code[pc];
    if ends(&i) {
        // `Jump` has gone already, and so has a native `Invoke`; the
        // interpreter's, like `Halt` and `Error`, never says to carry on.
        if i.op != Op::Jump {
            asm.ret(crate::abi::JUMPED);
        }
    } else if f.order.get(k + 1) != Some(&(pc + 1)) {
        book.sync(asm);
        book.entering(asm, f.loops.get(&(pc + 1)).copied());
        // Off the end of the program, this leaves without anywhere to go.
        f.goto(asm, k, pc + 1);
    } else {
        // Straight on into a loop's header.
        book.sync_steps_if_entering(asm, f.loops.get(&(pc + 1)).copied());
    }
}

/// The header words of the object the instruction at `pc` builds, if they can
/// be known when it is compiled: every field's descriptor is -- or, for an
/// array, they are all the same. Past `compact::INLINE_DESCS` fields the
/// header is longer than two words, and the words say so.
fn header(
    program: &Program,
    pc: usize,
    kind: crate::heap::Kind,
    meta: u32,
    n: usize,
) -> Option<Vec<u64>> {
    use meadow_bytecode::DESC_REG;
    let operands = program.operands(pc);
    let descs: Vec<meadow_rt::desc::Desc> = (0..n)
        .map(|k| {
            operands
                .get(k)
                .copied()
                .filter(|d| *d < DESC_REG)
                .map(|d| d as meadow_rt::desc::Desc)
        })
        .collect::<Option<_>>()?;
    if kind.is_uniform() {
        if descs.windows(2).any(|w| w[0] != w[1]) {
            return None;
        }
        // An array of bytes is packed, which the interpreter does.
        if kind == crate::heap::Kind::Array && descs.first() == Some(&crate::heap::Heap::BYTE) {
            return None;
        }
    }
    let mut words = vec![0u64; meadow_rt::compact::header_slots(kind.is_uniform(), n)];
    crate::object::write_header(kind, meta, descs.into_iter(), |k, w| words[k] = w);
    Some(words)
}

/// A typed `Int` instruction, whose division by zero the interpreter reports.
fn typed_int<E: Emit>(asm: &mut E, book: &mut Book, op: IntOp, i: Instr, c: Operand, pc: Pc) {
    let zero = asm.label();
    let done = asm.label();
    book.step(asm);
    let divides = matches!(op, IntOp::Div | IntOp::Rem);
    // Dividing can go to the interpreter, which has to see the machine as it
    // is.
    if divides {
        book.sync(asm);
    }
    asm.int(op, i.a, i.b, c, zero);
    book.wrote(i.a);
    if divides {
        asm.jump(done);
        asm.bind(zero);
        asm.unstep();
        asm.exec(pc);
        asm.bind(done);
    } else {
        asm.bind(zero);
        asm.bind(done);
    }
}

/// A constant as the word a register holds, if it is the same word in every
/// process: not an interned string, and not a `BigInt`, which is made.
fn immediate(program: &Program, k: u32) -> Option<u64> {
    let v = match program.consts.get(k as usize)? {
        Const::Unit => Value::Unit,
        Const::Bool(b) => Value::Bool(*b),
        Const::Int(n) => Value::Int(*n),
        Const::Float(x) => Value::Float(*x),
        Const::Word(w, b) => Value::Word(*w, *b),
        Const::Float32(x) => Value::Float32(*x),
        Const::Char(c) => Value::Char(*c),
        // Interned, and a key only in this process; or made on the heap.
        Const::Str(_) | Const::BigInt(_) | Const::Text(_) => return None,
    };
    Some(v.bits())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_layout_is_where_the_machine_is() {
        use super::layout::*;
        use std::mem::offset_of;
        type Vm = crate::Vm<'static>;
        assert_eq!(offset_of!(Vm, regs) as u32, REGS);
        assert_eq!(offset_of!(Vm, live) as u32, LIVE);
        assert_eq!(offset_of!(Vm, pc) as u32, PC);
        assert_eq!(offset_of!(Vm, at) as u32, AT);
        assert_eq!(offset_of!(Vm, steps) as u32, STEPS);
        assert_eq!(offset_of!(Vm, exec) as u32, EXEC);
        assert_eq!(offset_of!(Vm, method_pcs) as u32, METHOD_PCS);
        assert_eq!(offset_of!(Vm, method_starts) as u32, METHOD_STARTS);
        assert_eq!(offset_of!(Vm, native_table) as u32, NATIVE_TABLE);
        assert_eq!(offset_of!(Vm, native_methods) as u32, NATIVE_METHODS);
        let heap = offset_of!(Vm, heap) as u32;
        type Heap = crate::heap::Heap;
        assert_eq!(heap + offset_of!(Heap, base) as u32, BASE);
        assert_eq!(heap + offset_of!(Heap, cap) as u32, CAP);
        assert_eq!(heap + offset_of!(Heap, top) as u32, TOP);
        assert_eq!(heap + offset_of!(Heap, allocated) as u32, ALLOCATED);
        assert_eq!(heap + offset_of!(Heap, region_growth) as u32, REGION_GROWTH);
        assert_eq!(heap + offset_of!(Heap, tables) as u32, TABLES);
        assert_eq!(heap + offset_of!(Heap, fsp) as u32, FSP);
        assert_eq!(heap + offset_of!(Heap, flim) as u32, FLIM);
        assert_eq!(heap + offset_of!(Heap, fcur) as u32, FCUR);
        assert_eq!(heap + offset_of!(Heap, fbase) as u32, FBASE);
        assert_eq!(heap + offset_of!(Heap, fbias) as u32, FBIAS);
    }

    /// The shifts native code takes a heap address apart with have to be the
    /// ones the old generation is actually laid out by.
    #[test]
    fn an_address_comes_apart_where_the_heap_says_it_does() {
        use super::addr::*;
        assert_eq!(1 << GEN_SHIFT, crate::old::OLD_BASE, "the generation bit");
        assert_eq!(1 << (GEN_SHIFT + 1), crate::region::REGION_BASE);
        assert_eq!(1 << BLOCK_SHIFT, crate::old::BLOCK as u64, "block size");
        assert_eq!(SLOT_MASK, crate::old::BLOCK as u64 - 1);
        // Every old address has to fit the block field, or two blocks would
        // share a table entry.
        let top = (crate::region::REGION_BASE - crate::old::OLD_BASE) >> BLOCK_SHIFT;
        assert!(
            u64::from(top) <= BLOCK_MASK + 1,
            "{top} blocks do not fit the mask"
        );
    }
}
