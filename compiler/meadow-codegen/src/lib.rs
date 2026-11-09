//! AxCut → bytecode.
//!
//! The back end. Everything the sequent IR says about control — continuations,
//! handlers, the duality between calling and returning — is compiled away here,
//! and what comes out is a flat instruction array over a flat register file.
//! `meadow_glade` never learns any of it.
//!
//! # Why this pass is short
//!
//! Because AxCut already did the hard part. Its environment is an **ordered
//! list**, and a block's parameters name the whole of it, so at every program
//! point the set of live values and their order is written down in the IR. That
//! is the information a register allocator normally has to recover by computing
//! liveness, and here it is simply read off.
//!
//! What is left is bookkeeping of three kinds:
//!
//! * **Renaming is free.** `substitute [x, y] in {(a, b) => s}` emits *no code*
//!   at all when the block is laid out inline — it is a change of names, and the
//!   values are already in registers. Same for a `switch` arm and for the
//!   continuation of a primitive. Moves appear only at [`Statement::Jump`] and
//!   [`Statement::Invoke`], the two places control actually leaves — and often
//!   not even there. An instruction that wants a window of consecutive registers
//!   reads the environment where it already sits whenever the environment
//!   already has that shape, which it usually does: see [`Gen::gather`]. Reusing
//!   registers makes that a little less often true — an environment whose
//!   registers were reused is no longer in ascending order — and a window that
//!   has to be filled is the price of the moves that reuse removes. An
//!   instruction that builds an object pays nothing either way: where its
//!   fields are not one run it lists them instead ([`Gen::list`]).
//! * **A jump needs a permutation.** The target block wants its parameters in
//!   `r0..rn`, so [`Gen::parallel_move`] emits the moves — breaking cycles with
//!   one scratch register, which is the only place this pass has to think.
//! * **Blocks get laid out and patched.** A `new` puts its methods somewhere
//!   reachable and records their addresses in a method table.
//!
//! # Halting is an ordinary invoke
//!
//! Instruction 0 is `halt r1`, and the machine starts with `r0` holding a
//! closure whose one method is that instruction. A program's entry block takes
//! one parameter — the continuation to answer with — so "return from `main`"
//! needs no special case anywhere: it is the same `invoke` as every other
//! return, and it happens to land on a `halt`. The closure captures the
//! descriptor of the answer, which arrives after it, in `r1`.
//!
//! # Typed instructions
//!
//! A primitive whose operands are both `Int`, or both `Float`, is emitted as a
//! typed instruction -- `addi`, `cmpf`, `brik` -- that does its work on the
//! words themselves, with no operand descriptors and nothing to decode. The
//! representations come from the names (`meadow_seq::Rep`), so it is exactly
//! the code whose types are known that gets them: generic code keeps the
//! primitive, and specializing it is what turns it typed. See
//! [`Gen::typed_value`] and [`Gen::typed_test`].
//!
//! # Saying what registers hold
//!
//! A register is a word, so everything the machine has to know about one is
//! written beside the code: a register map at each instruction that can
//! collect, and operand descriptors at each that has to read a value --
//! see [`Gen::safepoint`] and [`Gen::operands_for`].
//!
//! # Registers are reused
//!
//! A name's register comes free at its last use, and the IR says where that is:
//! [`meadow_seq::still_used`] reads it off a statement rather than computing
//! liveness. At every binder the environment handed to what follows has the
//! names nothing below reads taken out of it, so the next `let`, `new` or
//! `extern` writes its result into one of them. The important case is that
//! **capturing a name is a use of it** — once a closure holds a copy, the
//! methods' reads are the methods' business — so a continuation built out of
//! the environment lands on top of what it closed over instead of one register
//! higher, and the transfer after it needs no permutation to put things back.
//! `fib`'s recursive call went from five instructions to three this way.
//!
//! There are two places it stops. A `jump` into a labelled block and an
//! `invoke` of a method hand the environment on whole, position for position,
//! to a block compiled separately against it — so nothing may be dropped on a
//! path that reaches one, which is what `still_used` answering `None` means. A
//! `substitute` then rebuilds a full environment out of its selection, and
//! from there the shapes `meadow_seq` believes in are true again. See
//! [`Gen::narrow`] and [`Gen::enter`].
//!
//! # Spilling
//!
//! An operand names one of 255 registers, and a block can have more values
//! live than that: an 800-element literal binds all its elements before it
//! builds anything, and generated code can hold hundreds of names across a
//! run of calls. The file has no memory behind it -- it is 256 words, and a
//! jump hands it over whole -- so what does not fit goes to the heap, with
//! instructions the machine already has:
//!
//! * **Spilling** packs a group of live values into a block (`MakeData`,
//!   described field by field like any other) held in one register, and
//!   frees theirs. It happens where a statement begins with more than
//!   [`SPILL_ABOVE`] registers in use, down to [`SPILL_TO`], and takes the
//!   values in the highest registers -- which also lowers the top of the
//!   file, where windows are laid. Never a descriptor: other values are read
//!   through those. See [`Gen::make_room`].
//! * **A window** -- the run of registers an array, a record, a primitive of
//!   three arguments or a match's fields is read from or loaded into -- has
//!   to be free in a row, not only free. Registers come free wherever names
//!   die, so a file a third full can have no run of 64 in it. Where a
//!   statement lays one and there is none, the values in the emptiest run
//!   are moved out of it first, and an `invoke` with nowhere to gather its
//!   arguments puts them at the bottom of the file as a `jump` does. See
//!   [`Window`].
//! * **Reloading** is a `Field` from the block, where a statement reads the
//!   value. A spilled value keeps its place in the environment, which is an
//!   ordered list -- a `jump` and an `invoke` hand it over by position -- and
//!   only where it is changes. Those two reload everything first.
//! * **A closure or frame** captures the spill blocks rather than what they
//!   hold, and its methods are entered knowing which captures are fields of
//!   which block: see [`Plan`]. So a non-tail call with three hundred values
//!   live builds a continuation of a few dozen.
//! * **An array literal** longer than [`ARRAY_CHUNK`] is built in pieces
//!   joined by `arrayConcat`, each piece's elements given up once it is built.
//! * **A constructor** of more than [`WIDE`] fields is made blank (`Blank`)
//!   and filled a field at a time by `setField`; **a record** that wide is
//!   made of its first fields and extended by the rest. Matching on such a
//!   constructor binds its fields where they are -- fields of the scrutinee,
//!   loaded when read -- rather than all at once.
//! * **A jump or an invoke** handing over more than [`ARGS_ABOVE`] values
//!   passes the first [`ARGS_KEPT`] in registers and the rest packed in
//!   blocks, and the block or method entered knows to look there: the same
//!   rule at both ends, [`arg_slots`], so that neither has to see the other.
//!
//! # What is not done yet
//!
//! **A continuation per non-tail call.** `meadow_seq` no longer builds one for
//! every subexpression — anything that cannot transfer control is lowered where
//! it stands, and a saturated call to a known function is a jump — so a tail
//! loop now allocates nothing at all. What remains is structural rather than an
//! oversight: a call in argument position has to record where to come back
//! to. That record used to be a heap object, four words allocated per call.
//! It is now a frame ([`Op::Frame`]) on a chunked frame stack, popped when the
//! function returns through it -- `meadow_seq::Program::frames` says which
//! `new`s are these. The worry about a stack was copying it whenever a
//! continuation is captured, which one-shot effect handlers do constantly;
//! chunks answer it, because a `handle` starts a chunk of its own and a
//! capture unlinks whole chunks instead of copying frames. See
//! `docs/RUNTIME.md`, "The call stack is a frame stack".

use meadow_axcut as seq;
use meadow_axcut::{Block, Extern, Label, Name, Rep, Statement};
use meadow_bytecode::{
    Cond, Const, DESC_REG, DescSrc, GcMap, Held, Instr, NO_MAP, NO_OPERANDS, NO_SOURCES, NameDesc,
    Op, Pc, Program, Reg,
};
use meadow_intern::InternedString;
use meadow_rt::desc::{self, Desc};
use meadow_rt::{Lit, Prim};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub msg: String,
}

fn err<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error { msg: msg.into() })
}

/// How many registers this pass will use.
///
/// One short of the 256 a `u8` operand can name. The top one is the runtime's
/// (`meadow_glade::vm::TEMP`): a fused compare-and-branch has to put its boolean
/// somewhere, and the instruction has no field left to say where.
const REGISTERS: usize = 255;

/// Compile an AxCut program to a loadable image.
pub fn compile(seq: &seq::Program) -> Result<Program, Error> {
    Gen::new(seq).run()
}

/// [`compile`], also recording the [`meadow_bytecode::DebugInfo`] a debugger
/// reads. The code is the same instruction for instruction; only the image
/// carries more.
pub fn compile_with_debug_info(seq: &seq::Program) -> Result<Program, Error> {
    let mut generator = Gen::new(seq);
    generator.debug = Some(Recorder::default());
    generator.run()
}

/// What [`compile_with_debug_info`] collects as it goes.
#[derive(Default)]
struct Recorder {
    locs: Vec<Option<meadow_rt::Loc>>,
    env_of: Vec<u32>,
    envs: Vec<Vec<(u32, Reg)>>,
    env_ids: HashMap<Vec<(u32, Reg)>, u32>,
    /// The environment the next instruction runs in.
    env: u32,
    /// The position the next instruction was written at.
    loc: Option<meadow_rt::Loc>,
    /// Per region: the definition it belongs to, and the position in effect
    /// where it was discovered. A continuation's first instructions belong to
    /// the call that made it until they say otherwise.
    region_name: Vec<InternedString>,
    region_loc: Vec<Option<meadow_rt::Loc>>,
    /// The region being emitted.
    current: usize,
    /// The environment last noted, and names from the environment around a
    /// `substitute` that only hands control over -- still in their registers
    /// until a move overwrites them, and what a debugger stopped there needs:
    /// the descriptors of the values being handed over among them.
    noted: Vec<(u32, Reg)>,
    carry: Vec<(u32, Reg)>,
}

/// Where a value is: a register, or a field of a spill block -- the block
/// being a value of the environment itself, under a name only this pass uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loc {
    Reg(Reg),
    Field(Name, u32),
}

/// The environment: what the IR calls each value, and where it is, in the
/// order the IR has them -- which a `jump` and an `invoke` hand over by
/// position. Spilling and reloading change where a value is, never its place.
#[derive(Debug, Clone, Default)]
struct Env {
    entries: Vec<(Name, Loc)>,
}

impl FromIterator<(Name, Reg)> for Env {
    fn from_iter<I: IntoIterator<Item = (Name, Reg)>>(it: I) -> Env {
        Env {
            entries: it.into_iter().map(|(n, r)| (n, Loc::Reg(r))).collect(),
        }
    }
}

impl Env {
    /// The values in registers, in order.
    fn regs(&self) -> impl Iterator<Item = (Name, Reg)> + '_ {
        self.entries.iter().filter_map(|(n, l)| match l {
            Loc::Reg(r) => Some((*n, *r)),
            Loc::Field(..) => None,
        })
    }

    fn loc(&self, n: Name) -> Option<Loc> {
        self.entries.iter().find(|(m, _)| *m == n).map(|(_, l)| *l)
    }

    fn reg(&self, n: Name) -> Option<Reg> {
        match self.loc(n) {
            Some(Loc::Reg(r)) => Some(r),
            _ => None,
        }
    }

    /// The name register `r` holds, if any.
    fn in_reg(&self, r: Reg) -> Option<Name> {
        self.regs().find(|(_, x)| *x == r).map(|(n, _)| n)
    }

    fn insert(&mut self, at: usize, n: Name, r: Reg) {
        self.entries.insert(at, (n, Loc::Reg(r)));
    }

    fn push(&mut self, n: Name, r: Reg) {
        self.entries.push((n, Loc::Reg(r)));
    }

    /// Whether anything is spilled.
    fn spills(&self) -> bool {
        self.entries
            .iter()
            .any(|(_, l)| matches!(l, Loc::Field(..)))
    }

    /// The spill blocks the entries in `locs` need, from `self` -- and the
    /// blocks those need, a block being a value that may be spilled in turn --
    /// leaving out any `locs` has already.
    fn blocks_for(&self, locs: &[(Name, Loc)]) -> Vec<(Name, Loc)> {
        let mut out: Vec<(Name, Loc)> = Vec::new();
        let mut todo: Vec<Loc> = locs.iter().map(|(_, l)| *l).collect();
        while let Some(l) = todo.pop() {
            if let Loc::Field(b, _) = l
                && !locs.iter().any(|(n, _)| *n == b)
                && !out.iter().any(|(n, _)| *n == b)
                && let Some(at) = self.loc(b)
            {
                out.push((b, at));
                todo.push(at);
            }
        }
        out
    }
}

fn reg_of(env: &Env, n: Name) -> Result<Reg, Error> {
    match env.loc(n) {
        Some(Loc::Reg(r)) => Ok(r),
        // A read that was not reloaded first: this pass's mistake, and one
        // that must not become a read of whatever the register holds.
        Some(Loc::Field(..)) => err(format!("{n:?} is spilled where it is read")),
        None => err(format!("{n:?} is not in scope during code generation")),
    }
}

fn regs_of(env: &Env) -> Vec<Reg> {
    env.regs().map(|(_, r)| r).collect()
}

/// A statement begins spilling when more registers than this are in use...
const SPILL_ABOVE: usize = 200;
/// ...and spills down to this many.
const SPILL_TO: usize = 150;
/// The longest array built by one instruction; longer literals are joined.
const ARRAY_CHUNK: usize = 64;
/// A constructor or record with more fields than this is built a piece at a
/// time, and a match on a constructor that wide loads its fields when read.
const WIDE: usize = 128;
/// A jump or invoke handing over more values than this packs the rest...
const ARGS_ABOVE: usize = 200;
/// ...keeping this many in registers...
const ARGS_KEPT: usize = 128;
/// ...and packing this many to a block.
const PACK: usize = 128;

/// Where each of `n` values handed over by a jump or an invoke arrives,
/// registers counted from `at`, when there are too many for registers: the
/// first [`ARGS_KEPT`] in registers, the rest as fields of blocks after them.
/// With how many registers that takes. `None` when they all fit.
///
/// Both ends go by this alone -- the jump or invoke packs by it, and the block
/// or method it enters unpacks by it -- which is what lets a method be entered
/// from anywhere and a labelled block be compiled once.
fn arg_slots(n: usize, at: usize) -> Option<(Vec<Slot>, usize)> {
    if n <= ARGS_ABOVE {
        return None;
    }
    let slots = (0..n)
        .map(|i| {
            if i < ARGS_KEPT {
                Slot::At((at + i) as Reg)
            } else {
                let p = (i - ARGS_KEPT) / PACK;
                Slot::In((at + ARGS_KEPT + p) as Reg, ((i - ARGS_KEPT) % PACK) as u32)
            }
        })
        .collect();
    Some((slots, ARGS_KEPT + (n - ARGS_KEPT).div_ceil(PACK)))
}
/// Where this pass's own names start: above every name lowering makes, the
/// synthetic ones included (`meadow_hir::SYNTHETIC_BASE`).
const OWN_NAMES: u32 = 0xF000_0000;

/// How a method's captures arrive when the `new` that built its object
/// captured spill blocks in place of what they hold.
#[derive(Debug, Clone)]
struct Plan {
    /// For each capture the method's block names, in order.
    slots: Vec<Slot>,
    /// How many values the object physically holds.
    physical: usize,
}

#[derive(Debug, Clone, Copy)]
enum Slot {
    /// The capture itself, at this position.
    At(Reg),
    /// A field of the spill block captured at this position.
    In(Reg, u32),
}

struct Gen<'a> {
    seq: &'a seq::Program,
    code: Vec<Instr>,

    consts: Vec<Const>,
    labels: Vec<InternedString>,
    shapes: Vec<Vec<InternedString>>,
    prims: Vec<Prim>,
    ops: Vec<(InternedString, InternedString)>,
    messages: Vec<String>,

    /// Blocks that need their own address, in discovery order.
    regions: Vec<&'a Block>,
    region_pc: Vec<Option<Pc>>,
    pending: Vec<usize>,
    /// Instructions whose immediate is a region's address, filled in at the end.
    fixups: Vec<(usize, usize)>,
    /// Method tables, as region ids until they are resolved.
    method_tables: Vec<Vec<usize>>,
    /// Alongside `method_tables`: see `Program::method_captures` and
    /// `Program::method_params`.
    method_captures: Vec<u8>,
    method_params: Vec<Vec<u8>>,
    label_region: HashMap<Label, usize>,

    max_reg: usize,
    /// Every name some other name's representation points at. A descriptor is
    /// a value like any other, but it is read through [`Gen::held`] rather
    /// than named in a statement, so [`Gen::narrow`] cannot see the use and
    /// must be told not to drop one.
    descriptors: std::collections::HashSet<u32>,
    debug: Option<Recorder>,
    /// Register maps for the instructions that may collect, each stored once.
    gc_maps: Vec<GcMap>,
    gc_index: HashMap<GcMap, u32>,
    /// `(pc, map)` per such instruction.
    gc_at: Vec<(usize, u32)>,
    /// Operand descriptors, and `(pc, where they start)` per instruction that
    /// has them.
    operands: Vec<DescSrc>,
    operands_at: Vec<(usize, u32)>,
    /// Field registers listed for an object-building instruction, and where
    /// each instruction's list starts: see `Program::sources`.
    sources: Vec<Reg>,
    sources_at: Vec<(usize, u32)>,
    /// Names this pass made up -- spill blocks, pieces of an array -- which
    /// hold references.
    own: std::collections::HashSet<Name>,
    /// Per region: how its captures arrive, when that is not in order.
    plans: Vec<Option<Plan>>,
}

impl<'a> Gen<'a> {
    fn new(seq: &'a seq::Program) -> Gen<'a> {
        Gen {
            seq,
            code: Vec::new(),
            consts: Vec::new(),
            labels: Vec::new(),
            shapes: Vec::new(),
            prims: Vec::new(),
            ops: Vec::new(),
            messages: Vec::new(),
            regions: Vec::new(),
            region_pc: Vec::new(),
            pending: Vec::new(),
            fixups: Vec::new(),
            method_tables: Vec::new(),
            method_captures: Vec::new(),
            method_params: Vec::new(),
            label_region: HashMap::new(),
            max_reg: 1,
            descriptors: seq
                .reps
                .values()
                .chain(seq.threads.values())
                .filter_map(|r| match r {
                    Rep::Var(d) if *d != seq::NO_DESC => Some(*d),
                    _ => None,
                })
                .collect(),
            debug: None,
            gc_maps: Vec::new(),
            gc_index: HashMap::new(),
            gc_at: Vec::new(),
            operands: Vec::new(),
            operands_at: Vec::new(),
            sources: Vec::new(),
            sources_at: Vec::new(),
            own: std::collections::HashSet::new(),
            plans: Vec::new(),
        }
    }

    fn run(mut self) -> Result<Program, Error> {
        // Instruction 0 is the whole of "the program finished". Method table 0
        // holds it, and the machine starts with a closure over that table in
        // `r0` — so a program's entry block, which takes its continuation as its
        // one parameter, needs nothing special to return to.
        //
        // The closure captures one thing: the descriptor of what the program
        // answers, which the machine knows when it starts it and a `halt`
        // cannot know from the instruction. So the answer arrives in `r1`.
        self.code.push(Instr::a(Op::Halt, 1));
        self.operands_for(&[DESC_REG]);
        self.method_tables.push(vec![usize::MAX]);
        self.method_captures.push(1);
        self.method_params.push(vec![2]);

        // Every top-level definition gets a region up front: a `jump` may name a
        // label whose block has not been reached yet.
        for def in &self.seq.defs {
            let id = self.region(&def.block);
            // Too many parameters for registers: entered the way a jump
            // hands them over (see `arg_slots`).
            if let Some((slots, physical)) = arg_slots(def.block.params.len(), 0) {
                self.plans[id] = Some(Plan { slots, physical });
            }
            if let Some(d) = &mut self.debug {
                d.region_name[id] = def.name;
            }
            self.label_region.insert(def.label, id);
        }

        while let Some(id) = self.pending.pop() {
            let block = self.regions[id];
            self.region_pc[id] = Some(self.code.len() as Pc);
            if let Some(d) = &mut self.debug {
                d.current = id;
                d.loc = d.region_loc[id];
            }
            // A region is entered with its parameters in r0..rn — that is what
            // `jump` and `invoke` arrange -- unless its object captured spill
            // blocks, when the plan says where each capture is.
            match self.plans[id].clone() {
                None => {
                    let vals: Vec<Reg> = (0..block.params.len() as u16).map(|i| i as Reg).collect();
                    self.track(block.params.len());
                    self.emit_block(block, &vals)?;
                }
                Some(plan) => {
                    let env = self.planned(block, &plan)?;
                    self.emit_stmt(&block.body, env)?;
                }
            }
        }

        for (at, region) in &self.fixups {
            let pc = self.region_pc[*region].ok_or_else(|| Error {
                msg: "a block was referenced but never emitted".into(),
            })?;
            self.code[*at].imm = pc;
        }

        let methods = self
            .method_tables
            .iter()
            .map(|table| {
                table
                    .iter()
                    .map(|id| {
                        // Table 0 is the halt continuation, whose one method is
                        // instruction 0.
                        if *id == usize::MAX {
                            Ok(0)
                        } else {
                            self.region_pc[*id].ok_or_else(|| Error {
                                msg: "a method was referenced but never emitted".into(),
                            })
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>()?;

        // `tags` is name -> tag; the runtime wants the other direction, and only
        // for printing.
        let mut ctors = vec![InternedString::from("?"); self.seq.tags.len()];
        for (name, tag) in &self.seq.tags {
            if let Some(slot) = ctors.get_mut(*tag as usize) {
                *slot = *name;
            }
        }

        // Indexed by label, so a test runner can start at a definition other
        // than `main`. `lower_program` hands back labels 0..n densely and sorted.
        let mut entries: Vec<(Label, Pc)> = self
            .seq
            .defs
            .iter()
            .map(|def| {
                let id = self.label_region[&def.label];
                (def.label, self.region_pc[id].expect("emitted above"))
            })
            .collect();
        entries.sort_by_key(|(l, _)| *l);
        let results: Vec<Desc> = entries.iter().map(|(l, _)| self.result(*l)).collect();
        let entries: Vec<Pc> = entries.into_iter().map(|(_, pc)| pc).collect();
        let entry_result = self.seq.entry.map_or(desc::ANY, |l| self.result(l));

        let entry = self
            .seq
            .entry
            .and_then(|l| self.label_region.get(&l).copied())
            .and_then(|id| self.region_pc[id]);

        let debug = self.debug.take().map(|mut d| {
            // Instruction 0, the halt, was emitted before anything was recorded.
            d.locs.resize(self.code.len(), None);
            d.env_of.resize(self.code.len(), 0);
            if d.envs.is_empty() {
                d.envs.push(Vec::new());
            }
            let mut starts: Vec<(Pc, usize)> = self
                .region_pc
                .iter()
                .enumerate()
                .filter_map(|(id, pc)| pc.map(|pc| (pc, id)))
                .collect();
            starts.sort();
            let regions = starts
                .iter()
                .enumerate()
                .map(|(i, &(entry, id))| meadow_bytecode::Region {
                    entry,
                    end: starts.get(i + 1).map_or(self.code.len() as Pc, |s| s.0),
                    name: d.region_name[id],
                    params: self.regions[id].params.iter().map(|n| n.0).collect(),
                    origin: d.region_loc[id],
                })
                .collect();
            Box::new(meadow_bytecode::DebugInfo {
                locs: d.locs,
                env_of: d.env_of,
                envs: d.envs,
                regions,
                returns: self.seq.returns.iter().map(|n| n.0).collect(),
                continuations: self.seq.continuations.iter().map(|n| n.0).collect(),
                origins: self.seq.origins.iter().map(|(c, o)| (c.0, o.0)).collect(),
                descs: self
                    .seq
                    .reps
                    .iter()
                    .filter_map(|(n, rep)| {
                        let d = match rep {
                            Rep::Var(d) if *d != seq::NO_DESC => NameDesc::Var(*d),
                            rep => NameDesc::Known(rep.desc()?),
                        };
                        Some((n.0, d))
                    })
                    .collect(),
            })
        });

        let mut gc_at = vec![NO_MAP; self.code.len()];
        for (pc, map) in &self.gc_at {
            gc_at[*pc] = *map;
        }
        let mut operands_at = vec![NO_OPERANDS; self.code.len()];
        for (pc, at) in &self.operands_at {
            operands_at[*pc] = *at;
        }
        let sources_at = if self.sources_at.is_empty() {
            Vec::new()
        } else {
            let mut at = vec![NO_SOURCES; self.code.len()];
            for (pc, k) in &self.sources_at {
                at[*pc] = *k;
            }
            at
        };
        Ok(Program {
            debug,
            sources_at,
            sources: self.sources,
            gc_maps: self.gc_maps,
            gc_at,
            operands_at,
            operands: self.operands,
            results,
            entry_result,
            code: self.code,
            consts: self.consts,
            methods,
            method_captures: self.method_captures.clone(),
            method_params: self.method_params.clone(),
            shapes: self.shapes,
            labels: self.labels,
            prims: self.prims,
            ops: self.ops,
            ctors,
            // In role order, so the image is the same every time.
            roles: {
                let mut roles: Vec<_> = self.seq.roles.iter().map(|(r, c)| (*r, *c)).collect();
                roles.sort_by_key(|(r, _)| r.index());
                roles
            },
            ctor_fields: self.seq.ctor_fields.clone(),
            messages: self.messages,
            entries,
            entry,
            regs: self.max_reg as u16,
        })
    }

    // --- what registers hold ---------------------------------------------

    /// What the value named `n` is, as the collector cares.
    ///
    /// A value of a type variable's type is described by a descriptor, which
    /// lowering keeps in every environment such a value is in (see
    /// `meadow_seq::describe`), so the map can say which register has it.
    fn held(&self, env: &Env, n: Name) -> Held {
        if self.own.contains(&n) {
            return Held::Ref;
        }
        match self.seq.reps.get(&n) {
            Some(Rep::Ref) => Held::Ref,
            Some(Rep::Int | Rep::Float | Rep::Bits(_) | Rep::Str) => Held::Scalar,
            Some(Rep::Var(d)) if *d != seq::NO_DESC => match env.reg(seq::VarId(*d)) {
                Some(r) => Held::Var(r),
                None => {
                    debug_assert!(false, "{n:?} is here without its descriptor");
                    Held::Any
                }
            },
            // A program lowered without representations, a name the lowering
            // could not type, or a type variable nothing binds: the collector
            // has to look.
            Some(Rep::Var(_) | Rep::Unknown) | None => Held::Any,
        }
    }

    /// What register `r` holds, going by the name the environment gives it.
    fn held_in(&self, env: &Env, r: Reg) -> Held {
        env.in_reg(r).map_or(Held::Any, |n| self.held(env, n))
    }

    /// The window `gather` filled for `srcs` at `base`, if it had to: copies of
    /// what the sources hold.
    fn gathered(&self, env: &Env, srcs: &[Reg], base: Reg) -> Vec<(Reg, Held)> {
        if run_of(srcs) == Some(base) {
            return Vec::new();
        }
        srcs.iter()
            .enumerate()
            .map(|(i, s)| (base + i as Reg, self.held_in(env, *s)))
            .collect()
    }

    /// The instruction just emitted may collect: record what every register
    /// the environment names holds there, and `extra` -- a window gathered for
    /// it, or a register it writes before it reads.
    fn safepoint(&mut self, env: &Env, extra: &[(Reg, Held)]) {
        let mut regs: std::collections::BTreeMap<Reg, Held> = std::collections::BTreeMap::new();
        for (n, r) in env.regs() {
            regs.insert(r, self.held(env, n));
        }
        for (r, h) in extra {
            regs.insert(*r, *h);
        }
        let map = GcMap {
            regs: regs.into_iter().collect(),
        };
        let id = match self.gc_index.get(&map) {
            Some(id) => *id,
            None => {
                let id = self.gc_maps.len() as u32;
                self.gc_index.insert(map.clone(), id);
                self.gc_maps.push(map);
                id
            }
        };
        self.gc_at.push((self.code.len() - 1, id));
    }

    // --- what operands are -----------------------------------------------

    /// Where the descriptor of the value named `n` is, in `env`.
    fn desc_src(&self, env: &Env, n: Name) -> DescSrc {
        if self.own.contains(&n) {
            return desc::REF as DescSrc;
        }
        match self.seq.reps.get(&n) {
            Some(Rep::Var(d)) if *d != seq::NO_DESC => match env.reg(seq::VarId(*d)) {
                Some(r) => DESC_REG + r as DescSrc,
                None => desc::ANY as DescSrc,
            },
            Some(rep) => rep.desc().unwrap_or(desc::ANY) as DescSrc,
            None => desc::ANY as DescSrc,
        }
    }

    /// The same for what register `r` holds.
    fn desc_in(&self, env: &Env, r: Reg) -> DescSrc {
        env.in_reg(r)
            .map_or(desc::ANY as DescSrc, |n| self.desc_src(env, n))
    }

    /// The operand descriptors of the instruction just emitted.
    fn operands_for(&mut self, srcs: &[DescSrc]) {
        let at = self.operands.len() as u32;
        self.operands.extend_from_slice(srcs);
        self.operands_at.push((self.code.len() - 1, at));
    }

    /// The descriptors of the values in registers `srcs`.
    fn operands_in(&mut self, env: &Env, srcs: &[Reg]) {
        let descs: Vec<DescSrc> = srcs.iter().map(|r| self.desc_in(env, *r)).collect();
        self.operands_for(&descs);
    }

    /// What the block labelled `l` answers, as a runtime starting it knows.
    fn result(&self, l: Label) -> Desc {
        self.seq
            .results
            .get(&l)
            .and_then(|rep| rep.desc())
            .unwrap_or(desc::ANY)
    }

    // --- tables -----------------------------------------------------------

    fn region(&mut self, block: &'a Block) -> usize {
        let id = self.regions.len();
        self.regions.push(block);
        self.plans.push(None);
        self.region_pc.push(None);
        self.pending.push(id);
        if let Some(d) = &mut self.debug {
            let name = d.region_name.get(d.current).copied().unwrap_or_default();
            d.region_name.push(name);
            d.region_loc.push(d.loc);
        }
        id
    }

    fn konst(&mut self, c: Const) -> u32 {
        match self.consts.iter().position(|k| *k == c) {
            Some(i) => i as u32,
            None => {
                self.consts.push(c);
                (self.consts.len() - 1) as u32
            }
        }
    }

    fn label(&mut self, l: InternedString) -> u32 {
        match self.labels.iter().position(|k| *k == l) {
            Some(i) => i as u32,
            None => {
                self.labels.push(l);
                (self.labels.len() - 1) as u32
            }
        }
    }

    fn prim(&mut self, p: Prim) -> u32 {
        match self.prims.iter().position(|k| *k == p) {
            Some(i) => i as u32,
            None => {
                self.prims.push(p);
                (self.prims.len() - 1) as u32
            }
        }
    }

    fn op(&mut self, e: InternedString, o: InternedString) -> u32 {
        match self.ops.iter().position(|k| *k == (e, o)) {
            Some(i) => i as u32,
            None => {
                self.ops.push((e, o));
                (self.ops.len() - 1) as u32
            }
        }
    }

    fn track(&mut self, high: usize) {
        self.max_reg = self.max_reg.max(high);
    }

    fn emit(&mut self, i: Instr) {
        self.record();
        self.code.push(i);
        if i.op == Op::Move
            && let Some(d) = &mut self.debug
            && d.carry.iter().any(|(_, r)| *r == i.a)
        {
            d.carry.retain(|(_, r)| *r != i.a);
            let noted = d.noted.clone();
            Self::note(d, noted);
        }
    }

    /// Emit an instruction whose immediate is a block's address, to be filled in
    /// once that block has been laid out.
    fn emit_to(&mut self, mut i: Instr, region: usize) {
        i.imm = 0;
        let at = self.code.len();
        self.record();
        self.code.push(i);
        self.fixups.push((at, region));
    }

    /// Note, for the instruction about to be emitted, where it came from and
    /// what the registers hold.
    fn record(&mut self) {
        let at = self.code.len();
        if let Some(d) = &mut self.debug {
            d.locs.resize(at, None);
            d.env_of.resize(at, 0);
            d.locs.push(d.loc);
            d.env_of.push(d.env);
        }
    }

    /// The environment the instructions emitted next run in.
    fn note_env(&mut self, env: &Env) {
        if let Some(d) = &mut self.debug {
            let key: Vec<(u32, Reg)> = env.regs().map(|(n, r)| (n.0, r)).collect();
            Self::note(d, key);
        }
    }

    fn note(d: &mut Recorder, env: Vec<(u32, Reg)>) {
        let mut key = env.clone();
        for &(n, r) in &d.carry {
            if !key.iter().any(|(m, x)| *m == n || *x == r) {
                key.push((n, r));
            }
        }
        d.noted = env;
        let next = d.envs.len() as u32;
        let id = *d.env_ids.entry(key.clone()).or_insert(next);
        if id == next {
            d.envs.push(key);
        }
        d.env = id;
    }

    // --- registers --------------------------------------------------------

    /// The lowest register the environment is not using.
    fn free(&mut self, env: &Env) -> Result<Reg, Error> {
        self.free_but(env, &[])
    }

    /// The lowest register neither the environment nor `keep` is using.
    ///
    /// `keep` is for the one instruction that cannot write over what it reads:
    /// [`Extern::PrimK`] parks its folded constant in the destination before
    /// the primitive runs. Every other instruction reads all of its operands
    /// before it writes, which is what makes it safe for a narrowed
    /// environment to hand back a register the instruction is about to read.
    fn free_but(&mut self, env: &Env, keep: &[Reg]) -> Result<Reg, Error> {
        let used: Vec<Reg> = regs_of(env);
        for r in 0..REGISTERS {
            let r = r as Reg;
            if !used.contains(&r) && !keep.contains(&r) {
                self.track(r as usize + 1);
                return Ok(r);
            }
        }
        err("no register is free, with the environment spilled")
    }

    /// The environment with the names `rest` will not read taken out of it.
    ///
    /// This is the whole of register reuse. A name's register becomes free at
    /// its *last* use, and capturing a name is a last use -- once a closure
    /// holds a copy, what its methods do with it later is the methods'
    /// business. Without this a straight-line block climbs through the file
    /// and every transfer out of it ends in a permutation; `move` was the
    /// single most-retired instruction in the machine because of it.
    ///
    /// [`seq::still_used`] answers `None` for a statement that hands its
    /// environment on whole -- a `jump` into a labelled block, an `invoke` of
    /// a method. Those blocks were compiled against the environment `lower`
    /// gave them, position for position, so nothing may be dropped on a path
    /// that reaches one, and `None` propagates up to every binder above.
    ///
    /// Everywhere else the only blocks between here and the next `substitute`
    /// are inline ones this pass lays out itself, and [`Gen::enter`] enters
    /// them by name rather than by position. The `substitute` then rebuilds a
    /// full environment out of its selection -- which is exactly the set of
    /// names still used -- so what `lower` and `meadow_seq::describe` believe
    /// about block shapes stays true from there on, and neither has to know
    /// this happened.
    fn narrow(&self, env: Env, rest: &Statement) -> Env {
        let Some(used) = seq::still_used(rest) else {
            return env;
        };
        let mut keep: std::collections::HashSet<Name> = env
            .entries
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| used.contains(n) || self.descriptors.contains(&n.0))
            .collect();
        // A spill block stays for as long as something it holds is wanted,
        // and so does the block it is in, if it is in one.
        loop {
            let more: Vec<Name> = env
                .entries
                .iter()
                .filter(|(n, _)| keep.contains(n))
                .filter_map(|(_, l)| match l {
                    Loc::Field(b, _) if !keep.contains(b) => Some(*b),
                    _ => None,
                })
                .collect();
            if more.is_empty() {
                break;
            }
            keep.extend(more);
        }
        Env {
            entries: env
                .entries
                .into_iter()
                .filter(|(n, _)| keep.contains(n))
                .collect(),
        }
    }

    /// A run of `n` registers above everything the environment holds.
    ///
    /// Above, rather than merely unused, so that filling it can be a plain
    /// sequence of moves with nothing to clobber. A destination register chosen
    /// by [`Gen::free`] may fall inside it, which is safe: every instruction
    /// that reads a window reads it before writing its destination.
    fn window(&mut self, env: &Env, n: usize) -> Result<Reg, Error> {
        let base = regs_of(env)
            .iter()
            .map(|r| *r as usize + 1)
            .max()
            .unwrap_or(0);
        if base + n <= REGISTERS {
            self.track(base + n);
            return Ok(base as Reg);
        }
        // Nothing above the environment: any run it does not use, which is
        // as safe -- filling it overwrites nothing the environment holds.
        // There is one: `make_room` saw to it, told of the window.
        free_run(&regs_of(env), n)
            .map(|b| {
                self.track(b + n);
                b as Reg
            })
            .ok_or_else(|| Error {
                msg: format!("no run of {n} free registers for a window"),
            })
    }

    /// Whether [`Gen::window`] finds a run of `n` registers in `env`.
    fn has_window(&self, env: &Env, n: usize) -> bool {
        let used = regs_of(env);
        let above = used.iter().map(|r| *r as usize + 1).max().unwrap_or(0);
        above + n <= REGISTERS || free_run(&used, n).is_some()
    }

    /// Where a windowed instruction should read its arguments from.
    ///
    /// A window is a run of consecutive registers, and `srcs` very often *is*
    /// one already: `invoke`'s arguments are the environment minus one slot, and
    /// a closure captures the environment whole, so both usually arrive as
    /// `r0, r1, r2, …` in order. Copying that somewhere else to satisfy the
    /// shape is pure waste — and it was most of what this pass emitted. Four
    /// moves before every `closure`, one before every `invoke`, all of them
    /// `move rN+1 <- rN`.
    ///
    /// So: if they are already consecutive and ascending, read them where they
    /// are; otherwise take a fresh window above the environment and fill it.
    /// Reading in place is safe for the same reason [`Gen::window`] is — every
    /// instruction that reads a window reads all of it before writing its
    /// destination, and the destination came from [`Gen::free`], so it is not
    /// one of the registers being read.
    fn gather(&mut self, env: &Env, srcs: &[Reg]) -> Result<Reg, Error> {
        if let Some(base) = run_of(srcs) {
            self.track(base as usize + srcs.len());
            return Ok(base);
        }
        let base = self.window(env, srcs.len())?;
        self.fill(base, srcs);
        Ok(base)
    }

    /// Where an object-building instruction -- `MakeData`, `Closure`, `Frame`
    /// -- reads its fields: its window's base when `srcs` already are one run,
    /// and otherwise nothing that matters, the registers being listed beside
    /// it by [`Gen::list`] instead.
    ///
    /// An object is only read into, so unlike an `invoke` it has no reason to
    /// want its operands anywhere in particular, and filling a window for one
    /// was the most common instruction the machine retired on closure-heavy
    /// code.
    fn fields(&mut self, srcs: &[Reg]) -> Reg {
        match run_of(srcs) {
            Some(base) => {
                self.track(base as usize + srcs.len());
                base
            }
            None => 0,
        }
    }

    /// List `srcs` as the fields of the instruction just emitted, unless
    /// [`Gen::fields`] found them a window.
    fn list(&mut self, srcs: &[Reg]) {
        if run_of(srcs).is_none() {
            let at = self.sources.len() as u32;
            self.sources.extend_from_slice(srcs);
            self.sources_at.push((self.code.len() - 1, at));
        }
    }

    /// Move `srcs` into a window at `base`, in order.
    fn fill(&mut self, base: Reg, srcs: &[Reg]) {
        for (i, s) in srcs.iter().enumerate() {
            let d = base + i as Reg;
            if d != *s {
                self.emit(Instr::new(Op::Move, d, *s, 0, 0));
            }
        }
    }

    /// Move `srcs[i]` into register `i`, for a jump into a block that wants its
    /// parameters at the bottom of the file.
    fn parallel_move(&mut self, srcs: &[Reg]) -> Result<(), Error> {
        let scratch = srcs
            .iter()
            .map(|r| *r as usize + 1)
            .max()
            .unwrap_or(0)
            .max(srcs.len());
        if scratch >= REGISTERS {
            return err("a jump needs a scratch register and the file is full");
        }
        let moves = order_moves(srcs, scratch as Reg);
        if !moves.is_empty() {
            self.track(scratch + 1);
        }
        for (d, s) in moves {
            self.emit(Instr::new(Op::Move, d, s, 0, 0));
        }
        Ok(())
    }

    // --- statements -------------------------------------------------------

    /// Enter a block laid out inline: the values it binds, and then the
    /// environment it continues -- that part by name, because the environment
    /// reaching here may be narrower than the one `lower` gave the block. A
    /// parameter with no register left is one [`Gen::narrow`] dropped, and
    /// nothing below reads it, so it enters holding nothing.
    fn enter(&mut self, block: &'a Block, bound: &[Reg], env: &Env) -> Result<(), Error> {
        if block.params.len() < bound.len() {
            return err(format!(
                "a block taking {} parameters binds {} values",
                block.params.len(),
                bound.len()
            ));
        }
        let mut child: Env = block
            .params
            .iter()
            .copied()
            .zip(bound.iter().copied())
            .collect();
        for p in &block.params[bound.len()..] {
            if let Some(l) = env.loc(*p) {
                child.entries.push((*p, l));
            }
        }
        let blocks = env.blocks_for(&child.entries);
        child.entries.extend(blocks);
        self.emit_stmt(&block.body, child)
    }

    fn emit_block(&mut self, block: &'a Block, vals: &[Reg]) -> Result<(), Error> {
        if block.params.len() != vals.len() {
            return err(format!(
                "block takes {} parameters but {} values reach it",
                block.params.len(),
                vals.len()
            ));
        }
        let env: Env = block
            .params
            .iter()
            .copied()
            .zip(vals.iter().copied())
            .collect();
        self.emit_stmt(&block.body, env)
    }

    fn emit_stmt(&mut self, s: &'a Statement, env: Env) -> Result<(), Error> {
        // Room for what this statement reads, in registers, and for the
        // window it may lay.
        let env = match s {
            Statement::Let { fields, .. } if fields.len() > WIDE => {
                self.make_room(env, &[], Window::No)?
            }
            Statement::Let { fields, .. } => self.make_room(env, fields, Window::No)?,
            Statement::Switch {
                scrutinee,
                arms,
                default,
            } => {
                // An arm binds its fields in a window, unless there are too
                // many to load at once.
                let fields = arms
                    .iter()
                    .filter_map(|(_, arm)| arm.params.len().checked_sub(default.params.len()))
                    .filter(|n| *n <= WIDE)
                    .max()
                    .unwrap_or(0);
                self.make_room(env, std::slice::from_ref(scrutinee), Window::Of(fields))?
            }
            // Its captures may stay where they are: see `Plan`.
            Statement::New { .. } => self.make_room(env, &[], Window::No)?,
            Statement::Extern { op, args, .. } => {
                if (matches!(op, Extern::Array) && args.len() > ARRAY_CHUNK)
                    || (matches!(op, Extern::Record(_)) && args.len() > WIDE)
                {
                    self.make_room(env, &[], Window::No)?
                } else {
                    // What `emit_extern` gathers: an array's elements, a
                    // record's fields, a primitive's arguments past two.
                    let window = match op {
                        Extern::Array | Extern::Record(_) => Window::OfReads,
                        Extern::Prim(_) if args.len() > 2 => Window::OfReads,
                        _ => Window::No,
                    };
                    self.make_room(env, args, window)?
                }
            }
            // Handing control over: everything comes back, in its place --
            // or, for more than a jump can hold, packed (see `Gen::hand_over`).
            Statement::Jump(_) | Statement::Invoke(..) => env,
            Statement::Mark(..) | Statement::Substitute(..) | Statement::Error(_) => env,
        };
        self.note_env(&env);
        match s {
            Statement::Mark(loc, inner) => {
                let outer = self.debug.as_mut().map(|d| d.loc.replace(*loc));
                let done = self.emit_stmt(inner, env);
                if let (Some(d), Some(outer)) = (&mut self.debug, outer) {
                    d.loc = outer;
                }
                done
            }
            // Pure renaming. The values are already where they are; the block
            // simply calls them something else.
            Statement::Substitute(sel, block) => {
                if block.params.len() != sel.len() {
                    return err(format!(
                        "block takes {} parameters but {} values reach it",
                        block.params.len(),
                        sel.len()
                    ));
                }
                let transfer = matches!(
                    peel_marks(&block.body),
                    Statement::Jump(_) | Statement::Invoke(..)
                );
                if transfer && let Some(d) = &mut self.debug {
                    d.carry = env.regs().map(|(n, r)| (n.0, r)).collect();
                }
                // A spilled value is renamed where it is.
                let mut child = Env::default();
                for (p, n) in block.params.iter().zip(sel) {
                    let Some(l) = env.loc(*n) else {
                        return err(format!("{n:?} is not in scope during code generation"));
                    };
                    child.entries.push((*p, l));
                }
                let blocks = env.blocks_for(&child.entries);
                child.entries.extend(blocks);
                let done = self.emit_stmt(&block.body, child);
                if let Some(d) = &mut self.debug {
                    d.carry.clear();
                }
                done
            }

            Statement::Jump(label) => {
                let Some(&region) = self.label_region.get(label) else {
                    return err(format!("jump to undefined label {label:?}"));
                };
                let (_, srcs) = self.hand_over(env, None)?;
                self.parallel_move(&srcs)?;
                let live = u8::try_from(srcs.len()).map_err(|_| Error {
                    msg: "a block takes more than 256 parameters".into(),
                })?;
                self.emit_to(Instr::new(Op::Jump, live, 0, 0, 0), region);
                Ok(())
            }

            Statement::Let {
                name,
                tag,
                fields,
                rest,
                ..
            } if fields.len() > WIDE => self.wide_data(*name, *tag, fields, rest, env),

            Statement::Let {
                name,
                tag,
                fields,
                rest,
                ..
            } => {
                let srcs = fields
                    .iter()
                    .map(|n| reg_of(&env, *n))
                    .collect::<Result<Vec<_>, _>>()?;
                let n = u8::try_from(srcs.len()).map_err(|_| Error {
                    msg: "a constructor with more than 256 fields".into(),
                })?;
                // The destination comes out of the narrowed environment, so
                // it may be a register this very instruction reads: `MakeData`
                // reads its whole window, and the descriptors beside it,
                // before it writes. The safepoint and the operand descriptors
                // are still taken from the environment as it stands, because
                // both describe the instruction's own read.
                let live = self.narrow(env.clone(), rest);
                let dst = self.free(&live)?;
                let base = self.fields(&srcs);
                self.emit(Instr::new(Op::MakeData, dst, base, n, *tag));
                self.list(&srcs);
                self.operands_in(&env, &srcs);
                self.safepoint(&env, &[]);
                let mut env = live;
                env.insert(0, *name, dst);
                self.emit_stmt(rest, env)
            }

            Statement::Switch {
                scrutinee,
                arms,
                default,
            } => {
                let scr = reg_of(&env, *scrutinee)?;
                for (tag, arm) in arms {
                    let tag16 = u16::try_from(*tag).map_err(|_| Error {
                        msg: format!("constructor tag {tag} does not fit a switch operand"),
                    })?;
                    let test = self.code.len();
                    self.emit(Instr::wide(Op::JumpUnlessTag, scr, tag16, 0));

                    // The arm binds the constructor's fields, then the
                    // environment as it stands — the scrutinee included. How
                    // many fields that is comes from the default, whose
                    // parameters are the environment and nothing else: the
                    // environment here may have been narrowed, so its own
                    // length no longer answers the question.
                    let nfields = arm
                        .params
                        .len()
                        .checked_sub(default.params.len())
                        .ok_or_else(|| Error {
                            msg: "a switch arm binding fewer values than the default".into(),
                        })?;
                    if nfields > WIDE {
                        // Too many to load at once: each stays a field of the
                        // scrutinee until something reads it. The scrutinee is
                        // held under a name of this pass's own, since the arm
                        // may not keep it -- and a name the arm does not have
                        // must not reach a jump out of it.
                        let holder = self.own_name();
                        let mut child = Env::default();
                        for (i, p) in arm.params[..nfields].iter().enumerate() {
                            child.entries.push((*p, Loc::Field(holder, i as u32)));
                        }
                        for p in &arm.params[nfields..] {
                            if let Some(l) = env.loc(*p) {
                                child.entries.push((*p, l));
                            }
                        }
                        child.push(holder, scr);
                        let blocks = env.blocks_for(&child.entries);
                        child.entries.extend(blocks);
                        self.emit_stmt(&arm.body, child)?;
                    } else {
                        let base = self.window(&env, nfields)?;
                        let mut fields = Vec::with_capacity(nfields);
                        for i in 0..nfields {
                            let d = base + i as Reg;
                            self.emit(Instr::new(Op::Field, d, scr, 0, i as u32));
                            fields.push(d);
                        }
                        self.enter(arm, &fields, &env)?;
                    }

                    // Every block body ends in a transfer, so the next
                    // instruction is where a failed test should land.
                    self.code[test].imm = self.code.len() as u32;
                }
                self.enter(default, &[], &env)
            }

            Statement::New {
                name,
                captures,
                methods,
                rest,
            } => {
                // What the object physically holds: each capture in a register,
                // and for those spilled, the block they are in -- once.
                let mut physical: Vec<Name> = Vec::new();
                let mut slots: Vec<Slot> = Vec::with_capacity(captures.len());
                for n in captures {
                    let at = |physical: &mut Vec<Name>, x: Name| {
                        physical.iter().position(|p| *p == x).unwrap_or_else(|| {
                            physical.push(x);
                            physical.len() - 1
                        }) as Reg
                    };
                    match env.loc(*n) {
                        Some(Loc::Reg(_)) => slots.push(Slot::At(at(&mut physical, *n))),
                        Some(Loc::Field(b, f)) => slots.push(Slot::In(at(&mut physical, b), f)),
                        None => {
                            return err(format!("{n:?} is not in scope during code generation"));
                        }
                    }
                }
                let srcs = physical
                    .iter()
                    .map(|n| reg_of(&env, *n))
                    .collect::<Result<Vec<_>, _>>()?;
                let ncap = u8::try_from(srcs.len()).map_err(|_| Error {
                    msg: "an object capturing more than 256 values".into(),
                })?;
                let planned = slots.iter().any(|s| matches!(s, Slot::In(..)));
                let table: Vec<usize> = methods.iter().map(|m| self.region(m)).collect();
                // Each method's registers: what the object holds, then its
                // arguments -- packed, past `ARGS_ABOVE` of them.
                let physcap = srcs.len();
                let mut params = Vec::with_capacity(methods.len());
                for (m, id) in methods.iter().zip(&table) {
                    let nargs = m.params.len() - captures.len();
                    let packed = arg_slots(nargs, physcap);
                    let width = physcap + packed.as_ref().map_or(nargs, |(_, w)| *w);
                    if planned || packed.is_some() {
                        let mut all = slots.clone();
                        match packed {
                            Some((s, _)) => all.extend(s),
                            None => all.extend((0..nargs).map(|j| Slot::At((physcap + j) as Reg))),
                        }
                        self.plans[*id] = Some(Plan {
                            slots: all,
                            physical: width,
                        });
                    }
                    params.push(u8::try_from(width).map_err(|_| Error {
                        msg: "a method taking more than 256 registers".into(),
                    })?);
                }
                let table_id = self.method_tables.len() as u32;
                self.method_tables.push(table);
                self.method_captures.push(ncap);
                self.method_params.push(params);

                // Capturing a name is the last use of it, so the new object
                // very often lands in a register one of its own captures was
                // holding. That is the point: the alternative is a closure
                // climbing one register higher than everything it closes over,
                // and a permutation at the next transfer to put it back.
                let live = self.narrow(env.clone(), rest);
                let dst = self.free(&live)?;
                let base = self.fields(&srcs);
                // A call's continuation goes on the frame stack; everything
                // else is an object. See `meadow_seq::Program::frames`.
                let op = if self.seq.frames.contains(name) {
                    Op::Frame
                } else {
                    Op::Closure
                };
                self.emit(Instr::new(op, dst, base, ncap, table_id));
                self.list(&srcs);
                self.operands_in(&env, &srcs);
                self.safepoint(&env, &[]);
                let mut env = live;
                env.insert(0, *name, dst);
                self.emit_stmt(rest, env)
            }

            Statement::Invoke(target, tag) => {
                let (env, srcs) = self.hand_over(env, Some(*target))?;
                let obj = reg_of(&env, *target)?;
                // A window for the arguments where the file has one. Where it
                // has not -- up to two hundred arguments, reloaded wherever
                // registers were free -- they go to the bottom of the file as
                // a jump's do, the object after them: nothing reads the
                // environment after an invoke, so all of it may be written.
                let (obj, base) = if run_of(&srcs).is_some() || self.has_window(&env, srcs.len()) {
                    (obj, self.gather(&env, &srcs)?)
                } else {
                    let mut all = srcs.clone();
                    all.push(obj);
                    self.parallel_move(&all)?;
                    self.track(all.len());
                    (srcs.len() as Reg, 0)
                };
                let method = u8::try_from(*tag).map_err(|_| Error {
                    msg: format!("method {tag} does not fit an invoke operand"),
                })?;
                self.emit(Instr::new(Op::Invoke, obj, method, base, srcs.len() as u32));
                Ok(())
            }

            Statement::Extern { op, args, blocks } => self.emit_extern(op, args, blocks, env),

            Statement::Error(msg) => {
                let id = self.messages.len() as u32;
                self.messages.push((*msg).to_string());
                self.emit(Instr::i(Op::Error, id));
                Ok(())
            }
        }
    }

    /// The representation of the value named `n`.
    fn rep_of(&self, n: Name) -> Rep {
        self.seq.reps.get(&n).copied().unwrap_or(Rep::Unknown)
    }

    /// A value-producing primitive as a typed instruction into `dst`, if its
    /// operands' representations allow one. False, having emitted nothing, if
    /// not.
    fn typed_value(
        &mut self,
        op: &Extern,
        args: &[Name],
        srcs: &[Reg],
        dst: Reg,
        env: &Env,
    ) -> Result<bool, Error> {
        match (op, args, srcs) {
            // `popCount` and `toFloat` on an `Int`, which are one instruction
            // each on every machine this targets. A matrix of pixels computes
            // its coordinates with `toFloat` twice per pixel, and a hash trie
            // finds a child with `popCount` at every level, so these two were
            // the most-run primitives left in the interpreter after arrays.
            (Extern::Prim(p), [x], [rx]) => {
                let Some(op) = typed1(*p, self.rep_of(*x)) else {
                    return Ok(false);
                };
                self.emit(Instr::new(op, dst, *rx, 0, 0));
                Ok(true)
            }
            (Extern::Prim(p), [x, y], [rx, ry]) => {
                let Some(t) = typed(*p, self.rep_of(*x), self.rep_of(*y)) else {
                    return Ok(false);
                };
                self.emit(match t {
                    Typed::Int(op, _) | Typed::Float(op) => Instr::new(op, dst, *rx, *ry, 0),
                    Typed::Cmp(c) => Instr::new(Op::CmpI, dst, *rx, *ry, c as u32),
                    Typed::FloatCmp(c) => Instr::new(Op::CmpF, dst, *rx, *ry, c as u32),
                });
                Ok(true)
            }
            (Extern::PrimK(p, l), [x], [rx]) => {
                let (lrep, bits) = lit_rep(l);
                let Some(t) = typed(*p, self.rep_of(*x), lrep) else {
                    return Ok(false);
                };
                let small = bits.and_then(|b| i32::try_from(b as i64).ok());
                match (t, small) {
                    (Typed::Int(_, Some(k)), Some(n)) => {
                        self.emit(Instr::new(k, dst, *rx, 0, n as u32));
                        return Ok(true);
                    }
                    (Typed::Cmp(c), Some(n)) => {
                        self.emit(Instr::new(Op::CmpIK, dst, *rx, c as u8, n as u32));
                        return Ok(true);
                    }
                    _ => {}
                }
                // The constant into a register of its own -- one the
                // destination may be, which is safe: the operation reads both
                // before it writes.
                let tmp = self.window(env, 1)?;
                let k = self.konst(constant(l));
                self.emit(Instr::ai(Op::Const, tmp, k));
                self.emit(match t {
                    Typed::Int(op, _) | Typed::Float(op) => Instr::new(op, dst, *rx, tmp, 0),
                    Typed::Cmp(c) => Instr::new(Op::CmpI, dst, *rx, tmp, c as u32),
                    Typed::FloatCmp(c) => Instr::new(Op::CmpF, dst, *rx, tmp, c as u32),
                });
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// A branching `extern` as a typed compare-and-branch, if its operands'
    /// representations allow one: where its false jump is, to be patched.
    fn typed_test(
        &mut self,
        op: &Extern,
        args: &[Name],
        srcs: &[Reg],
        env: &Env,
    ) -> Result<Option<usize>, Error> {
        let (t, x, y) = match (op, args, srcs) {
            (Extern::BranchPrim(p), [x, y], [rx, ry]) => {
                let Some(t) = typed(*p, self.rep_of(*x), self.rep_of(*y)) else {
                    return Ok(None);
                };
                (t, *rx, *ry)
            }
            (Extern::BranchPrimK(p, l), [x], [rx]) => {
                let (lrep, bits) = lit_rep(l);
                let Some(t) = typed(*p, self.rep_of(*x), lrep) else {
                    return Ok(None);
                };
                if let (Typed::Cmp(c), Some(n)) =
                    (t, bits.and_then(|b| i8::try_from(b as i64).ok()))
                {
                    self.emit(Instr::new(Op::BrIK, *rx, n as u8, c as u8, 0));
                    return Ok(Some(self.code.len() - 1));
                }
                let tmp = self.free(env)?;
                let k = self.konst(constant(l));
                self.emit(Instr::ai(Op::Const, tmp, k));
                (t, *rx, tmp)
            }
            _ => return Ok(None),
        };
        let instr = match t {
            Typed::Cmp(c) => Instr::new(Op::BrI, x, y, c as u8, 0),
            Typed::FloatCmp(c) => Instr::new(Op::BrF, x, y, c as u8, 0),
            // Arithmetic is not a test.
            Typed::Int(..) | Typed::Float(_) => return Ok(None),
        };
        self.emit(instr);
        Ok(Some(self.code.len() - 1))
    }

    /// Emit the test a branching `extern` performs, and answer where the jump
    /// that takes the false arm ended up — its immediate still has to be
    /// patched once that arm's address is known.
    ///
    /// Usually one instruction. The exception is a literal comparison whose
    /// constant does not fit the operand byte the fused form has for it, which
    /// loads the constant and compares the two registers instead. Same answer,
    /// one instruction more, and no cliff in what the compiler will accept.
    fn emit_test(&mut self, op: &Extern, srcs: &[Reg], env: &Env) -> Result<usize, Error> {
        let at = |g: &Gen| g.code.len() - 1;
        match op {
            Extern::Branch => {
                let [c] = srcs else {
                    return err(format!("a branch needs 1 argument, got {}", srcs.len()));
                };
                self.emit(Instr::ai(Op::JumpUnless, *c, 0));
                Ok(at(self))
            }
            Extern::BranchPrim(p) => {
                let [x, y] = srcs else {
                    return err(format!(
                        "a branch on {p:?} needs 2 arguments, got {}",
                        srcs.len()
                    ));
                };
                fusable(*p)?;
                let id = self.prim(*p);
                self.emit(Instr::new(Op::JumpUnlessPrim, *x, *y, prim_byte(id)?, 0));
                self.operands_in(env, &[*x, *y]);
                Ok(at(self))
            }
            Extern::BranchPrimK(p, l) => {
                let [x] = srcs else {
                    return err(format!(
                        "a branch on {p:?} needs 1 argument, got {}",
                        srcs.len()
                    ));
                };
                fusable(*p)?;
                let id = self.prim(*p);
                let k = self.konst(constant(l));
                let operands = [self.desc_in(env, *x), const_desc(&constant(l)) as DescSrc];
                // Loading the constant may allocate -- a `BigInt` -- so both
                // ways can collect.
                match u8::try_from(k) {
                    Ok(k) => {
                        self.emit(Instr::new(Op::JumpUnlessPrimK, *x, k, prim_byte(id)?, 0));
                        self.operands_for(&operands);
                        self.safepoint(env, &[]);
                    }
                    Err(_) => {
                        let tmp = self.free(env)?;
                        self.emit(Instr::ai(Op::Const, tmp, k));
                        self.safepoint(env, &[]);
                        self.emit(Instr::new(Op::JumpUnlessPrim, *x, tmp, prim_byte(id)?, 0));
                        self.operands_for(&operands);
                    }
                }
                Ok(at(self))
            }
            other => err(format!("{other:?} is not a branch")),
        }
    }

    // --- spilling ----------------------------------------------------------

    /// A name of this pass's own, holding a reference.
    fn own_name(&mut self) -> Name {
        let n = seq::VarId(OWN_NAMES + self.own.len() as u32);
        self.own.insert(n);
        n
    }

    /// `env` with `reads` in registers, spilling first if the file is too
    /// full for what the statement will want -- see the module docs,
    /// "Spilling".
    ///
    /// `window` says what run of free registers the statement may lay a
    /// window in ([`Gen::window`]): for that, there have to be so many
    /// registers free *and* free in a row. The first is a matter of how much is
    /// live, and spilling answers it. The second is not: registers come free
    /// wherever names die, and a file a third full can have no run of 64 in it
    /// -- every third register held, say, by the elements of an array literal
    /// each computed in three. So where there is no such run, the values in
    /// the way are moved out of it ([`Gen::open_run`]).
    fn make_room(&mut self, env: Env, reads: &[Name], window: Window) -> Result<Env, Error> {
        let spilled_reads = reads
            .iter()
            .filter(|n| matches!(env.loc(**n), Some(Loc::Field(..))))
            .count();
        let mut env = env;
        let run = match window {
            Window::No => 0,
            // Reads already in a run are read where they are.
            Window::OfReads if in_place(&env, reads) => 0,
            Window::OfReads => reads.len(),
            Window::Of(n) => n,
        };
        let wanted = env.regs().count() + spilled_reads;
        if wanted > SPILL_ABOVE || wanted + run > REGISTERS {
            // Down far enough that what is to be reloaded fits as well, and
            // the window beside it -- and the block spilled to.
            let to = SPILL_TO
                .min(SPILL_ABOVE.saturating_sub(spilled_reads))
                .min(REGISTERS.saturating_sub(run + spilled_reads + 1));
            env = self.spill(env, reads, to)?;
        }
        for n in reads {
            self.reload(&mut env, *n)?;
        }
        // Reloaded, the reads may have come to be a run themselves.
        let in_a_run = matches!(window, Window::OfReads) && in_place(&env, reads);
        if run > 0 && !in_a_run {
            env = self.open_run(env, run)?;
        }
        Ok(env)
    }

    /// `env` with a run of `n` registers it does not use, moving values out of
    /// the way if it has none.
    ///
    /// The run taken is the one with the fewest values in it, the highest of
    /// those, so that the moves are few and the bottom of the file, where a
    /// jump wants its values, is left alone. A value moved changes where it
    /// is, never its place in the environment, as with a reload.
    fn open_run(&mut self, mut env: Env, n: usize) -> Result<Env, Error> {
        let used = regs_of(&env);
        if free_run(&used, n).is_some() {
            return Ok(env);
        }
        let Some(top) = REGISTERS.checked_sub(n) else {
            return err(format!("a window of {n} registers is wider than the file"));
        };
        let held = |b: usize| (b..b + n).filter(|r| used.contains(&(*r as Reg))).count();
        let base = (0..=top)
            .rev()
            .min_by_key(|b| held(*b))
            .expect("a file at least as wide as the window has a run");
        let run = base..base + n;
        let mut taken = used.clone();
        let mut moving: Vec<Reg> = used
            .iter()
            .copied()
            .filter(|r| run.contains(&(*r as usize)))
            .collect();
        moving.sort_unstable();
        moving.dedup();
        for from in moving {
            let Some(to) = (0..REGISTERS)
                .map(|r| r as Reg)
                .find(|r| !run.contains(&(*r as usize)) && !taken.contains(r))
            else {
                return err(format!(
                    "no run of {n} free registers, with {} of them in use",
                    used.len()
                ));
            };
            self.track(to as usize + 1);
            self.emit(Instr::new(Op::Move, to, from, 0, 0));
            taken.push(to);
            for e in env.entries.iter_mut() {
                if e.1 == Loc::Reg(from) {
                    e.1 = Loc::Reg(to);
                }
            }
        }
        Ok(env)
    }

    /// Spill the values in the highest registers until `to` are in use,
    /// keeping `keep`, the descriptors, and the blocks themselves.
    fn spill(&mut self, mut env: Env, keep: &[Name], to: usize) -> Result<Env, Error> {
        let mut victims: Vec<(Name, Reg)> = env
            .regs()
            .filter(|(n, _)| {
                !keep.contains(n) && !self.descriptors.contains(&n.0) && !self.own.contains(n)
            })
            .collect();
        victims.sort_by_key(|(_, r)| std::cmp::Reverse(*r));
        let over = env.regs().count().saturating_sub(to);
        victims.truncate(over.max(1).min(255));
        if victims.is_empty() {
            return Ok(env);
        }
        let srcs: Vec<Reg> = victims.iter().map(|(_, r)| *r).collect();
        let dst = self.free(&env)?;
        let base = self.fields(&srcs);
        self.emit(Instr::new(Op::MakeData, dst, base, srcs.len() as u8, 0));
        self.list(&srcs);
        self.operands_in(&env, &srcs);
        self.safepoint(&env, &[]);
        let block = self.own_name();
        for (i, (n, _)) in victims.iter().enumerate() {
            if let Some(e) = env.entries.iter_mut().find(|(m, _)| m == n) {
                e.1 = Loc::Field(block, i as u32);
            }
        }
        env.push(block, dst);
        Ok(env)
    }

    /// Bring `n` back into a register, if it was spilled.
    fn reload(&mut self, env: &mut Env, n: Name) -> Result<(), Error> {
        let Some(Loc::Field(block, field)) = env.loc(n) else {
            return Ok(());
        };
        // The block may itself have been spilled, or be a field of something.
        self.reload(env, block)?;
        let from = reg_of(env, block)?;
        let dst = self.free(env)?;
        self.emit(Instr::new(Op::Field, dst, from, 0, field));
        for e in env.entries.iter_mut().filter(|(m, _)| *m == n) {
            e.1 = Loc::Reg(dst);
        }
        Ok(())
    }

    /// Everything back in registers and the blocks gone: what a `jump` or an
    /// `invoke` hands over, position for position.
    fn reload_all(&mut self, mut env: Env) -> Result<Env, Error> {
        if !env.spills() {
            return Ok(env);
        }
        let spilled: Vec<Name> = env
            .entries
            .iter()
            .filter(|(_, l)| matches!(l, Loc::Field(..)))
            .map(|(n, _)| *n)
            .collect();
        for n in spilled {
            self.reload(&mut env, n)?;
        }
        env.entries.retain(|(n, _)| !self.own.contains(n));
        Ok(env)
    }

    /// The environment a method starts in when its object captured spill
    /// blocks: each capture where [`Plan`] says, then its arguments.
    fn planned(&mut self, block: &'a Block, plan: &Plan) -> Result<Env, Error> {
        let ncap = plan.slots.len();
        if block.params.len() < ncap {
            return err("a method taking fewer parameters than its object captured");
        }
        let mut env = Env::default();
        let mut blocks: Vec<(Reg, Name)> = Vec::new();
        for (p, slot) in block.params.iter().zip(&plan.slots) {
            match *slot {
                Slot::At(r) => env.push(*p, r),
                Slot::In(r, f) => {
                    let b = match blocks.iter().find(|(x, _)| *x == r) {
                        Some((_, b)) => *b,
                        None => {
                            let b = self.own_name();
                            blocks.push((r, b));
                            b
                        }
                    };
                    env.entries.push((*p, Loc::Field(b, f)));
                }
            }
        }
        for (r, b) in blocks {
            env.push(b, r);
        }
        for (i, p) in block.params[ncap..].iter().enumerate() {
            env.push(*p, (plan.physical + i) as Reg);
        }
        self.track(plan.physical + block.params.len() - ncap);
        Ok(env)
    }

    /// What a `jump` or an `invoke` hands over: every value of the environment
    /// the IR has, in order, but `except` -- the object invoked. Answers the
    /// environment, with `except` in a register, and where each value to hand
    /// over is: in registers, or past [`ARGS_ABOVE`] of them the first
    /// [`ARGS_KEPT`] in registers and the rest packed by [`arg_slots`]' rule.
    fn hand_over(&mut self, env: Env, except: Option<Name>) -> Result<(Env, Vec<Reg>), Error> {
        let names: Vec<Name> = env
            .entries
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| !self.own.contains(n) && Some(*n) != except)
            .collect();
        if names.len() <= ARGS_ABOVE {
            let env = self.reload_all(env)?;
            let srcs = names
                .iter()
                .map(|n| reg_of(&env, *n))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok((env, srcs));
        }
        let mut env = env;
        let mut packs: Vec<Name> = Vec::new();
        for chunk in names[ARGS_KEPT..].chunks(PACK) {
            env = self.make_room(env, chunk, Window::No)?;
            let srcs = chunk
                .iter()
                .map(|n| reg_of(&env, *n))
                .collect::<Result<Vec<_>, _>>()?;
            let dst = self.free(&env)?;
            let base = self.fields(&srcs);
            self.emit(Instr::new(Op::MakeData, dst, base, srcs.len() as u8, 0));
            self.list(&srcs);
            self.operands_in(&env, &srcs);
            self.safepoint(&env, &[]);
            // Packed: what is handed over is the pack.
            env.entries.retain(|(n, _)| !chunk.contains(n));
            let pack = self.own_name();
            env.push(pack, dst);
            packs.push(pack);
        }
        let kept: Vec<Name> = names[..ARGS_KEPT]
            .iter()
            .copied()
            .chain(packs.iter().copied())
            .collect();
        let reads: Vec<Name> = kept.iter().copied().chain(except).collect();
        env = self.make_room(env, &reads, Window::No)?;
        let srcs = kept
            .iter()
            .map(|n| reg_of(&env, *n))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((env, srcs))
    }

    /// Take `gone` out of `env`, and with them the blocks of this pass's own
    /// that nothing left -- nor `keep` -- is in.
    fn give_up(&self, env: &mut Env, gone: &[Name], keep: &[Name]) {
        env.entries.retain(|(n, _)| !gone.contains(n));
        let mut need: std::collections::HashSet<Name> = keep.iter().copied().collect();
        loop {
            let more: Vec<Name> = env
                .entries
                .iter()
                .filter(|(n, _)| !self.own.contains(n) || need.contains(n))
                .filter_map(|(_, l)| match l {
                    Loc::Field(b, _) if !need.contains(b) => Some(*b),
                    _ => None,
                })
                .collect();
            if more.is_empty() {
                break;
            }
            need.extend(more);
        }
        env.entries
            .retain(|(n, _)| !self.own.contains(n) || need.contains(n));
    }

    /// Which of `done` nothing reads any more: not `later` values still to be
    /// used here, nor what follows (`after`), nor a descriptor.
    fn finished(
        &self,
        done: &[Name],
        later: &[Name],
        after: &Option<std::collections::HashSet<Name>>,
    ) -> Vec<Name> {
        done.iter()
            .copied()
            .filter(|n| {
                !later.contains(n)
                    && after.as_ref().is_some_and(|w| !w.contains(n))
                    && !self.descriptors.contains(&n.0)
            })
            .collect()
    }

    /// `let name = tag(fields) in rest`, for more fields than one instruction
    /// takes: made blank, and filled by `setField` one field at a time, so
    /// that only the field being written need be in a register.
    fn wide_data(
        &mut self,
        name: Name,
        tag: u32,
        fields: &'a [Name],
        rest: &'a Statement,
        env: Env,
    ) -> Result<(), Error> {
        let n = fields.len();
        let wide = u16::try_from(n).map_err(|_| Error {
            msg: format!("a constructor of {n} fields"),
        })?;
        let mut env = env;
        let dst = self.free(&env)?;
        self.emit(Instr::new(
            Op::Blank,
            dst,
            (wide >> 8) as u8,
            wide as u8,
            tag,
        ));
        self.safepoint(&env, &[]);
        let obj = self.own_name();
        env.insert(0, obj, dst);
        let set = self.prim(Prim::SetField);
        let after = seq::still_used(rest);
        for (k, chunk) in fields.chunks(ARRAY_CHUNK).enumerate() {
            for (j, f) in chunk.iter().enumerate() {
                env = self.make_room(env, &[*f, obj], Window::Of(3))?;
                let (o, v) = (reg_of(&env, obj)?, reg_of(&env, *f)?);
                // `setField obj i v`, its operands in a window above everything
                // live, the index loaded straight into it.
                let base = self.window(&env, 3)?;
                self.emit(Instr::new(Op::Move, base, o, 0, 0));
                let at = self.konst(Const::Int((k * ARRAY_CHUNK + j) as i64));
                self.emit(Instr::ai(Op::Const, base + 1, at));
                self.emit(Instr::new(Op::Move, base + 2, v, 0, 0));
                let out = self.free(&env)?;
                self.emit(Instr::new(Op::Prim, out, base, 3, set));
                self.operands_for(&[
                    desc::REF as DescSrc,
                    desc::INT as DescSrc,
                    self.desc_src(&env, *f),
                ]);
                let window = [
                    (base, Held::Ref),
                    (base + 1, Held::Scalar),
                    (base + 2, self.held(&env, *f)),
                ];
                self.safepoint(&env, &window);
            }
            let later = fields.get((k + 1) * ARRAY_CHUNK..).unwrap_or(&[]);
            let gone = self.finished(chunk, later, &after);
            self.give_up(&mut env, &gone, &[obj]);
        }
        let dst = reg_of(&env, obj)?;
        env.entries.retain(|(m, _)| *m != obj);
        let mut env = self.narrow(env, rest);
        env.insert(0, name, dst);
        self.emit_stmt(rest, env)
    }

    /// A record literal of more fields than one instruction takes: made of
    /// its first [`ARRAY_CHUNK`], and extended by the rest one at a time.
    fn wide_record(
        &mut self,
        labels: &[InternedString],
        args: &'a [Name],
        block: &'a Block,
        env: Env,
    ) -> Result<(), Error> {
        let after = seq::still_used(&block.body);
        let (first, rest) = args.split_at(ARRAY_CHUNK);
        let mut env = self.make_room(env, first, Window::OfReads)?;
        let srcs = first
            .iter()
            .map(|n| reg_of(&env, *n))
            .collect::<Result<Vec<_>, _>>()?;
        let shape = self.shapes.len() as u32;
        self.shapes.push(labels[..ARRAY_CHUNK].to_vec());
        let base = self.gather(&env, &srcs)?;
        let dst = self.free(&env)?;
        self.emit(Instr::new(
            Op::MakeRecord,
            dst,
            base,
            srcs.len() as u8,
            shape,
        ));
        self.operands_in(&env, &srcs);
        let window = self.gathered(&env, &srcs, base);
        self.safepoint(&env, &window);
        let mut acc = self.own_name();
        env.insert(0, acc, dst);
        let gone = self.finished(first, rest, &after);
        self.give_up(&mut env, &gone, &[acc]);
        for (i, (label, x)) in labels[ARRAY_CHUNK..].iter().zip(rest).enumerate() {
            env = self.make_room(env, &[*x, acc], Window::No)?;
            let (r, v) = (reg_of(&env, acc)?, reg_of(&env, *x)?);
            let out = self.free(&env)?;
            let id = self.label(*label);
            self.emit(Instr::new(Op::Extend, out, r, v, id));
            self.operands_in(&env, &[r, v]);
            self.safepoint(&env, &[]);
            env.entries.retain(|(m, _)| *m != acc);
            acc = self.own_name();
            env.insert(0, acc, out);
            let gone = self.finished(&[*x], &rest[i + 1..], &after);
            self.give_up(&mut env, &gone, &[acc]);
        }
        let dst = reg_of(&env, acc)?;
        let mut live = self.narrow(env, &block.body);
        live.entries.retain(|(n, _)| *n != acc);
        self.enter(block, &[dst], &live)
    }

    /// An array literal longer than one instruction builds: in pieces of
    /// [`ARRAY_CHUNK`], each joined onto what came before by `arrayConcat`,
    /// and each piece's elements given up once it is built, so that however
    /// long the literal, only a piece of it is in registers at a time.
    fn big_array(&mut self, args: &'a [Name], block: &'a Block, env: Env) -> Result<(), Error> {
        let mut env = env;
        let wanted_after = seq::still_used(&block.body);
        let concat = self.prim(Prim::ArrayConcat);
        let mut acc: Option<Name> = None;
        for (k, chunk) in args.chunks(ARRAY_CHUNK).enumerate() {
            env = self.make_room(env, chunk, Window::OfReads)?;
            let srcs = chunk
                .iter()
                .map(|n| reg_of(&env, *n))
                .collect::<Result<Vec<_>, _>>()?;
            let base = self.gather(&env, &srcs)?;
            let part = self.free(&env)?;
            self.emit(Instr::new(Op::MakeArray, part, base, srcs.len() as u8, 0));
            self.operands_in(&env, &srcs);
            let window = self.gathered(&env, &srcs, base);
            self.safepoint(&env, &window);
            let piece = self.own_name();
            env.insert(0, piece, part);
            acc = Some(match acc {
                None => piece,
                Some(before) => {
                    let (x, y) = (reg_of(&env, before)?, part);
                    let out = self.free(&env)?;
                    self.emit(Instr::new(Op::Prim2, out, x, y, concat));
                    self.operands_for(&[desc::REF as DescSrc, desc::REF as DescSrc]);
                    self.safepoint(&env, &[]);
                    env.entries.retain(|(n, _)| *n != before && *n != piece);
                    let joined = self.own_name();
                    env.insert(0, joined, out);
                    joined
                }
            });
            // The elements built in are given up, unless something still
            // reads them -- a later piece, or what follows.
            let rest = args.get((k + 1) * ARRAY_CHUNK..).unwrap_or(&[]);
            let wanted = |n: &Name| {
                rest.contains(n)
                    || wanted_after.as_ref().is_none_or(|w| w.contains(n))
                    || self.descriptors.contains(&n.0)
            };
            let gone: Vec<Name> = chunk.iter().copied().filter(|n| !wanted(n)).collect();
            env.entries.retain(|(n, _)| !gone.contains(n));
            // A spill block nothing refers to any more goes too.
            let held: Vec<Name> = env
                .entries
                .iter()
                .filter_map(|(_, l)| match l {
                    Loc::Field(b, _) => Some(*b),
                    Loc::Reg(_) => None,
                })
                .collect();
            let acc_now = acc;
            env.entries
                .retain(|(n, _)| !self.own.contains(n) || held.contains(n) || Some(*n) == acc_now);
        }
        let acc = acc.expect("a literal longer than a piece has pieces");
        let dst = reg_of(&env, acc)?;
        let mut live = self.narrow(env, &block.body);
        live.entries.retain(|(n, _)| *n != acc);
        self.enter(block, &[dst], &live)
    }

    fn emit_extern(
        &mut self,
        op: &'a Extern,
        args: &'a [Name],
        blocks: &'a [Block],
        env: Env,
    ) -> Result<(), Error> {
        // A branch is the only kind of extern that does not produce a value, and
        // the only one with two continuations. None of the three changes the
        // environment.
        if op.is_branch() {
            let [on_false, on_true] = blocks else {
                return err(format!(
                    "a branch needs 2 continuations, got {}",
                    blocks.len()
                ));
            };
            let srcs = args
                .iter()
                .map(|n| reg_of(&env, *n))
                .collect::<Result<Vec<_>, _>>()?;
            let test = match self.typed_test(op, args, &srcs, &env)? {
                Some(test) => test,
                None => self.emit_test(op, &srcs, &env)?,
            };
            self.enter(on_true, &[], &env)?;
            self.code[test].imm = self.code.len() as u32;
            return self.enter(on_false, &[], &env);
        }

        let [block] = blocks else {
            return err(format!(
                "a value-producing extern needs 1 continuation, got {}",
                blocks.len()
            ));
        };
        if matches!(op, Extern::Array) && args.len() > ARRAY_CHUNK {
            return self.big_array(args, block, env);
        }
        if let Extern::Record(labels) = op
            && args.len() > WIDE
        {
            return self.wide_record(labels, args, block, env);
        }
        let srcs = args
            .iter()
            .map(|n| reg_of(&env, *n))
            .collect::<Result<Vec<_>, _>>()?;
        // What the continuation still reads. The operands are not in it if
        // this is their last use, and every extern but one reads all of them
        // before it writes, so the result may land on one of them -- which is
        // what makes `n - 1` in a loop write over `n`. The exception is the
        // untyped folded primitive below, which takes its own destination.
        let live = self.narrow(env.clone(), &block.body);
        let mut dst = self.free(&live)?;

        if self.typed_value(op, args, &srcs, dst, &env)? {
            return self.enter(block, &[dst], &live);
        }

        match op {
            Extern::Branch | Extern::BranchPrim(_) | Extern::BranchPrimK(_, _) => {
                unreachable!("handled above")
            }
            Extern::Lit(l) => {
                let k = self.konst(constant(l));
                self.emit(Instr::ai(Op::Const, dst, k));
                self.safepoint(&env, &[]);
            }
            Extern::Native(effect, op) => {
                let [x] = srcs[..] else {
                    return err(format!("{effect}.{op} takes 1 argument"));
                };
                let id = self.op(*effect, *op);
                self.emit(Instr::new(Op::Native, dst, x, 0, id));
                self.operands_in(&env, &[x]);
                self.safepoint(&env, &[]);
            }
            Extern::Prim(p) => {
                let id = self.prim(*p);
                let mut operands: Vec<DescSrc> =
                    srcs.iter().map(|r| self.desc_in(&env, *r)).collect();
                // A thread's function answers what the thread does, and the
                // machine that runs it has to be told what that is.
                if *p == Prim::ThreadSpawn {
                    let out = block.params[0];
                    let answer = match self.seq.threads.get(&out) {
                        Some(Rep::Var(d)) if *d != seq::NO_DESC => env
                            .reg(seq::VarId(*d))
                            .map_or(desc::ANY as DescSrc, |r| DESC_REG + r as DescSrc),
                        Some(rep) => rep.desc().unwrap_or(desc::ANY) as DescSrc,
                        None => desc::ANY as DescSrc,
                    };
                    operands.push(answer);
                }
                // One and two arguments name their registers directly. Only
                // arity three still gathers a window, and there are four such
                // primitives — it is not worth a fourth operand field that
                // every other instruction would carry unused.
                match srcs[..] {
                    [x] => {
                        self.emit(Instr::new(Op::Prim1, dst, x, 0, id));
                        self.operands_for(&operands);
                        self.safepoint(&env, &[]);
                    }
                    [x, y] => {
                        self.emit(Instr::new(Op::Prim2, dst, x, y, id));
                        self.operands_for(&operands);
                        self.safepoint(&env, &[]);
                    }
                    _ => {
                        let n = srcs.len() as u8;
                        let base = self.gather(&env, &srcs)?;
                        self.emit(Instr::new(Op::Prim, dst, base, n, id));
                        self.operands_for(&operands);
                        let window = self.gathered(&env, &srcs, base);
                        self.safepoint(&env, &window);
                    }
                }
            }
            // A literal operand does not reach a register at all: the
            // instruction carries the constant index.
            Extern::PrimK(p, l) => {
                let [x] = srcs[..] else {
                    return err(format!("{p:?} with a folded constant takes 1 argument"));
                };
                // The runtime parks the constant in the destination before
                // running the primitive, so this is the one instruction whose
                // destination may not be its operand. It can be: a narrowed
                // environment hands back the registers of names this extern
                // was the last use of, and `x` is often one. So take another.
                if dst == x {
                    dst = self.free_but(&live, &srcs)?;
                }
                let id = self.prim(*p);
                let k = self.konst(constant(l));
                let operands = [self.desc_in(&env, x), const_desc(&constant(l)) as DescSrc];
                if matches!(l, Lit::BigInt(_) | Lit::Str(_)) {
                    // Loading a `BigInt` allocates, and the destination is not
                    // written until it has: there is no register to hold it
                    // then. So it gets a register of its own.
                    let tmp = self.window(&env, 1)?;
                    self.emit(Instr::ai(Op::Const, tmp, k));
                    self.safepoint(&env, &[]);
                    self.emit(Instr::new(Op::Prim2, dst, x, tmp, id));
                    self.operands_for(&operands);
                    self.safepoint(&env, &[(tmp, Held::Ref)]);
                } else {
                    self.emit(Instr::new(Op::PrimK, dst, x, prim_byte(id)?, k));
                    self.operands_for(&operands);
                    // The constant waits in the destination while the
                    // primitive runs.
                    self.safepoint(&env, &[(dst, Held::Scalar)]);
                }
            }
            Extern::Array => {
                let n = u8::try_from(srcs.len()).map_err(|_| Error {
                    msg: "an array literal of more than 256 elements".into(),
                })?;
                let base = self.gather(&env, &srcs)?;
                self.emit(Instr::new(Op::MakeArray, dst, base, n, 0));
                self.operands_in(&env, &srcs);
                let window = self.gathered(&env, &srcs, base);
                self.safepoint(&env, &window);
            }
            Extern::Record(fields) => {
                let n = u8::try_from(srcs.len()).map_err(|_| Error {
                    msg: "a record of more than 256 fields".into(),
                })?;
                let id = self.shapes.len() as u32;
                self.shapes.push(fields.clone());
                let base = self.gather(&env, &srcs)?;
                self.emit(Instr::new(Op::MakeRecord, dst, base, n, id));
                self.operands_in(&env, &srcs);
                let window = self.gathered(&env, &srcs, base);
                self.safepoint(&env, &window);
            }
            Extern::Select(l, _) => {
                let id = self.label(*l);
                self.emit(Instr::new(Op::Select, dst, srcs[0], 0, id));
            }
            Extern::Extend(l, _) => {
                let id = self.label(*l);
                self.emit(Instr::new(Op::Extend, dst, srcs[0], srcs[1], id));
                self.operands_in(&env, &[srcs[0], srcs[1]]);
                self.safepoint(&env, &[]);
            }
            Extern::Field(i) => {
                self.emit(Instr::new(Op::Field, dst, srcs[0], 0, *i as u32));
            }
        }

        self.enter(block, &[dst], &live)
    }
}

/// A statement with the positions around it taken off.
fn peel_marks(s: &Statement) -> &Statement {
    match s {
        Statement::Mark(_, inner) => peel_marks(inner),
        s => s,
    }
}

/// The base of `srcs` if it is already an ascending run of consecutive
/// registers — which is what a window wants, and what the environment usually
/// hands over.
///
/// The empty list is a run at register 0: nothing is read, so where it would
/// have been read from does not matter.
fn run_of(srcs: &[Reg]) -> Option<Reg> {
    let first = match srcs.first() {
        None => return Some(0),
        Some(r) => *r,
    };
    // A run cannot reach past the file; `window` would have refused too.
    if first as usize + srcs.len() > REGISTERS {
        return None;
    }
    srcs.iter()
        .enumerate()
        .all(|(i, r)| *r == first + i as Reg)
        .then_some(first)
}

/// What run of free registers a statement may lay a window in
/// ([`Gen::window`]), beside the registers of what it reads: what
/// [`Gen::make_room`] must leave room for.
#[derive(Debug, Clone, Copy)]
enum Window {
    No,
    /// One its reads are copied into, in order -- unless they are such a run
    /// already, and are read where they are ([`Gen::gather`]).
    OfReads,
    /// One of this many registers, loaded with something else: a match's
    /// fields, `setField`'s operands.
    Of(usize),
}

/// Whether `reads` are in a run of registers, in order: what
/// [`Gen::gather`] reads where it is, with no window.
fn in_place(env: &Env, reads: &[Name]) -> bool {
    reads
        .iter()
        .map(|r| env.reg(*r))
        .collect::<Option<Vec<_>>>()
        .is_some_and(|srcs| run_of(&srcs).is_some())
}

/// The lowest base of a run of `n` registers none of `used` is in.
fn free_run(used: &[Reg], n: usize) -> Option<usize> {
    (0..=REGISTERS.checked_sub(n)?).find(|&b| (b..b + n).all(|r| !used.contains(&(r as Reg))))
}

/// Order the moves that put `srcs[i]` into register `i`, as `(dst, src)` pairs.
///
/// The one genuinely fiddly thing in this pass, and the reason it is a free
/// function with tests of its own: sources and destinations overlap, so the
/// moves have to be sequenced, and a cycle (`r0 ← r1`, `r1 ← r0`) has no valid
/// sequence at all. Emit whatever is safe, and when nothing is, park one value
/// in `scratch` — that breaks the cycle and the rest falls out.
///
/// A wrong answer here is not a crash. It is a program that runs and computes
/// something else.
fn order_moves(srcs: &[Reg], scratch: Reg) -> Vec<(Reg, Reg)> {
    let mut pending: Vec<(Reg, Reg)> = srcs
        .iter()
        .enumerate()
        .filter(|(i, s)| **s != *i as Reg)
        .map(|(i, s)| (*s, i as Reg))
        .collect();
    let mut out = Vec::new();

    while !pending.is_empty() {
        // A move is safe when nothing still pending reads its destination.
        let ready = pending
            .iter()
            .position(|(_, d)| !pending.iter().any(|(s, _)| s == d));
        match ready {
            Some(i) => {
                let (s, d) = pending.remove(i);
                out.push((d, s));
            }
            None => {
                let (s, _) = pending[0];
                out.push((scratch, s));
                for (src, _) in pending.iter_mut() {
                    if *src == s {
                        *src = scratch;
                    }
                }
            }
        }
    }
    out
}

// --- typed instructions ------------------------------------------------------

/// What a primitive on operands of known representation becomes.
#[derive(Debug, Clone, Copy)]
enum Typed {
    /// An `Int` operation: the register form, and the one taking a constant.
    Int(Op, Option<Op>),
    Float(Op),
    /// A comparison of words -- `Int` order, or equality of any immediate but a
    /// float.
    Cmp(Cond),
    FloatCmp(Cond),
}

/// A one-operand primitive as a typed instruction, if the operand's
/// representation allows one.
fn typed1(p: Prim, x: Rep) -> Option<Op> {
    if x != Rep::Int {
        return None;
    }
    Some(match p.untyped() {
        Prim::PopCount => Op::PopI,
        Prim::ToFloat => Op::ItoF,
        _ => return None,
    })
}

/// Can two values of representation `rep` be told equal by their words? Every
/// immediate but a float: an interned symbol is its key, a sized integer its
/// masked bits.
fn word_equal(rep: Rep) -> bool {
    match rep {
        Rep::Int | Rep::Str => true,
        Rep::Bits(d) => d != desc::FLOAT32,
        _ => false,
    }
}

/// The typed form of `p` on operands represented as `x` and `y`, if there is
/// one.
fn typed(p: Prim, x: Rep, y: Rep) -> Option<Typed> {
    use Prim::*;
    let p = p.untyped();
    let ints = x == Rep::Int && y == Rep::Int;
    let floats = x == Rep::Float && y == Rep::Float;
    let cond = |p: Prim| match p {
        Eq => Cond::Eq,
        Ne => Cond::Ne,
        Lt | LtF => Cond::Lt,
        Le | LeF => Cond::Le,
        Gt | GtF => Cond::Gt,
        _ => Cond::Ge,
    };
    Some(match p {
        Add if ints => Typed::Int(Op::AddI, Some(Op::AddIK)),
        Sub if ints => Typed::Int(Op::SubI, Some(Op::SubIK)),
        Mul if ints => Typed::Int(Op::MulI, Some(Op::MulIK)),
        Div if ints => Typed::Int(Op::DivI, None),
        Mod if ints => Typed::Int(Op::ModI, None),
        // The shift count and the mask are `Int`s like the value, so `ints`
        // covers both operands. A mask too wide for the immediate field falls
        // back to the register form, which is what `small` being `None` in
        // [`Gen::typed_value`] already means.
        Shl if ints => Typed::Int(Op::ShlI, Some(Op::ShlIK)),
        Shr if ints => Typed::Int(Op::ShrI, Some(Op::ShrIK)),
        BitAnd if ints => Typed::Int(Op::AndI, Some(Op::AndIK)),
        Ushr if ints => Typed::Int(Op::UshrI, Some(Op::UshrIK)),
        AddF if floats => Typed::Float(Op::AddF),
        SubF if floats => Typed::Float(Op::SubF),
        MulF if floats => Typed::Float(Op::MulF),
        DivF if floats => Typed::Float(Op::DivF),
        Lt | Le | Gt | Ge if ints => Typed::Cmp(cond(p)),
        LtF | LeF | GtF | GeF if floats => Typed::FloatCmp(cond(p)),
        Eq | Ne if floats => Typed::FloatCmp(cond(p)),
        Eq | Ne if x == y && word_equal(x) => Typed::Cmp(cond(p)),
        _ => return None,
    })
}

/// A literal's representation, where it is fixed; and its word, where that is
/// the same in every process -- which an interned symbol's is not, and a
/// string's, an address, never is.
fn lit_rep(l: &Lit) -> (Rep, Option<u64>) {
    match l {
        Lit::Int(n) => (Rep::Int, Some(*n as u64)),
        Lit::Float(x) => (Rep::Float, Some(x.to_bits())),
        Lit::Str(_) => (Rep::Ref, None),
        Lit::Sym(_) => (Rep::Str, None),
        Lit::Char(c) => (Rep::Bits(desc::CHAR), Some(*c as u64)),
        Lit::Bool(b) => (Rep::Bits(desc::BOOL), Some(*b as u64)),
        Lit::Unit => (Rep::Bits(desc::UNIT), Some(0)),
        Lit::Word(w, b) => (Rep::Bits(desc::word(*w)), Some(*b)),
        Lit::BigInt(_) | Lit::AnyInt(..) | Lit::AnyFloat(..) | Lit::Float32(_) => {
            (Rep::Unknown, None)
        }
    }
}

/// What a constant is.
fn const_desc(c: &Const) -> Desc {
    match c {
        Const::Unit => desc::UNIT,
        Const::Bool(_) => desc::BOOL,
        Const::Int(_) => desc::INT,
        Const::Float(_) => desc::FLOAT,
        Const::Word(w, _) => desc::word(*w),
        Const::Float32(_) => desc::FLOAT32,
        Const::Str(_) => desc::STR,
        Const::Char(_) => desc::CHAR,
        Const::BigInt(_) | Const::Text(_) => desc::REF,
    }
}

fn constant(l: &Lit) -> Const {
    match l {
        Lit::Int(n) | Lit::AnyInt(n, _) => Const::Int(*n),
        Lit::BigInt(n) => Const::BigInt(*n),
        Lit::Float(x) | Lit::AnyFloat(x, _) => Const::Float(*x),
        Lit::Word(w, b) => Const::Word(*w, *b),
        Lit::Float32(x) => Const::Float32(*x),
        Lit::Str(s) => Const::Text(*s),
        Lit::Sym(s) => Const::Str(*s),
        Lit::Char(c) => Const::Char(*c),
        Lit::Bool(b) => Const::Bool(*b),
        Lit::Unit => Const::Unit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a move sequence against a register file and see where things land.
    fn apply(srcs: &[Reg], scratch: Reg) -> Vec<u32> {
        // Each register starts holding a distinguishable value.
        let mut regs: Vec<u32> = (0..=scratch as u32 + 1).collect();
        for (d, s) in order_moves(srcs, scratch) {
            regs[d as usize] = regs[s as usize];
        }
        regs
    }

    #[track_caller]
    fn lands_correctly(srcs: &[Reg]) {
        let scratch = srcs
            .iter()
            .map(|r| *r + 1)
            .max()
            .unwrap_or(0)
            .max(srcs.len() as Reg);
        let regs = apply(srcs, scratch);
        for (i, s) in srcs.iter().enumerate() {
            assert_eq!(
                regs[i], *s as u32,
                "register {i} should hold what r{s} held, for {srcs:?}"
            );
        }
    }

    #[test]
    fn a_move_that_needs_no_moves_emits_none() {
        assert!(order_moves(&[0, 1, 2], 3).is_empty());
        assert!(order_moves(&[], 0).is_empty());
    }

    #[test]
    fn moves_are_ordered_so_nothing_is_clobbered() {
        // r0 <- r1, r1 <- r2: doing them in the written order would lose r1.
        lands_correctly(&[1, 2, 3]);
        lands_correctly(&[3, 2, 1, 0]);
        lands_correctly(&[2, 0, 1]);
    }

    #[test]
    fn a_cycle_is_broken_with_the_scratch_register() {
        // A swap has no valid order at all — one value has to be parked.
        lands_correctly(&[1, 0]);
        let moves = order_moves(&[1, 0], 2);
        assert!(
            moves.iter().any(|(d, _)| *d == 2),
            "a swap should use the scratch register: {moves:?}"
        );
        // Two independent cycles, and a cycle with a tail hanging off it.
        lands_correctly(&[1, 0, 3, 2]);
        lands_correctly(&[1, 2, 0]);
        lands_correctly(&[1, 2, 0, 0]);
    }

    #[test]
    fn arguments_already_in_a_row_are_read_where_they_are() {
        // What `invoke` and `closure` are handed nearly always: a prefix or a
        // suffix of the environment, in order.
        assert_eq!(run_of(&[]), Some(0));
        assert_eq!(run_of(&[7]), Some(7));
        assert_eq!(run_of(&[0, 1, 2, 3]), Some(0));
        assert_eq!(run_of(&[5, 6]), Some(5));

        // And what is not a window: a gap, a descent, a repeat.
        assert_eq!(run_of(&[0, 2]), None);
        assert_eq!(run_of(&[3, 2]), None);
        assert_eq!(run_of(&[1, 1]), None);
        // A run that would reach past the file is not one.
        assert_eq!(run_of(&[255, 0]), None);
    }

    #[test]
    fn duplicated_sources_all_arrive() {
        // `f f x` puts the same value in two places, and reading a register
        // twice has to keep working after the first write.
        lands_correctly(&[2, 2, 2]);
        lands_correctly(&[1, 1, 0]);
    }
}

/// The primitive table index as an operand byte.
///
/// The folded instructions name their primitive in `c`, which is a `u8`. There
/// are fewer than a hundred primitives in the language, so this cannot fail —
/// but a table that grew past 256 would otherwise wrap silently into a
/// different operation, and that is not a failure worth discovering at run
/// time.
fn prim_byte(id: u32) -> Result<u8, Error> {
    u8::try_from(id).map_err(|_| Error {
        msg: format!("primitive {id} does not fit a folded instruction's operand"),
    })
}

/// A fused compare-and-branch keeps its boolean in the one register the
/// runtime reserves, and that register is **not** a collector root — so the
/// primitive must not allocate. [`Prim::compares`] is the list of ones that do
/// not, and this is the check that the lowering only ever built a fused branch
/// from one of them.
///
/// A guard rather than a fallback: the two crates would have to disagree about
/// what a comparison is for this to fire, and quietly generating slower code
/// would hide that.
fn fusable(p: Prim) -> Result<(), Error> {
    if p.compares() {
        return Ok(());
    }
    err(format!(
        "{p:?} was fused into a branch, but only a comparison can be — it would \
         leave its result where the collector cannot see it"
    ))
}
