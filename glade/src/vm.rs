//! The virtual machine.
//!
//! A program counter, a flat register file and a collected heap. That is the
//! whole of it — there is **no `call` and no `ret`**, and no handler stack
//! either: the compiler passes effect handlers as evidence, so a `handle` and
//! a `perform` arrive here as ordinary objects and jumps, and only an
//! operation no handler answers reaches the machine, as [`Op::Native`].
//!
//! # Why there is no `call`
//!
//! Because the compiler already removed the need for one. In the sequent IR the
//! back end works from, a function is handed the continuation it should answer,
//! and returning is entering that continuation. A closure, a continuation and a
//! handler are the same kind of object, and [`Op::Invoke`] does the same
//! thing to all three: rebuild the register file as the object's captures
//! followed by its arguments, and jump. A tail call is not an optimisation
//! here; it is what an ordinary call already is.
//!
//! # The frame stack
//!
//! What a call that is *not* in tail position needs is a continuation to come
//! back to, and [`Op::Frame`] makes one: laid out as a closure, so `Invoke`
//! enters it as it enters anything else, but written into the thread's frame
//! stack -- 64 KiB chunks of the heap's old generation, linked, so a deep
//! recursion grows a chain and copies nothing -- and reclaimed by moving the
//! stack's top back when the function returns through it. It is not the
//! machine's stack: it belongs to the heap, the collector walks it as it walks
//! the registers, and a `handle` starts a chunk of its own so that a
//! resumption can take the frames above it away and put them back. See
//! `docs/RUNTIME.md`, "The call stack is a frame stack".
//!
//! # Registers and the collector
//!
//! One flat file of 256 registers, no frame pointer. The compiler uses 255 of
//! them; the top one is [`TEMP`], where a fused compare-and-branch puts the
//! boolean it is about to throw away. The collector's root set is
//! `r0..live`, and `live` is maintained by the instructions that know it:
//! [`Op::Jump`] carries the target block's arity, [`Op::Invoke`] sets it from the
//! object's captures plus its arguments, and any instruction writing `r[a]`
//! raises it to `a + 1`. Registers above `live` are dead by construction; ones
//! below it may be stale, which retains a bounded amount of garbage and is the
//! price of not emitting liveness metadata per instruction.
//!
//! # A register is a word
//!
//! Nothing in a register says what it is: an `Int`, a `Float` and an address
//! are all 64 bits. What the compiler knows about them it writes down beside
//! the code, and the machine reads it where it needs it:
//!
//! * **what an operand is.** An instruction that has to know -- a primitive, an
//!   object built from registers, a `halt` -- has operand descriptors
//!   ([`meadow_bytecode::Program::operands`]): a descriptor, or the register
//!   holding one when the value's type is a type variable. [`Vm::value`] reads
//!   a register as the [`Value`] they say, and the primitives, `show` and the
//!   natives go on working with values. An instruction that only moves a word
//!   -- `move`, `field`, `invoke` -- never asks, and neither does a typed one
//!   -- `addi`, `brf` -- whose opcode says what its operands are.
//! * **which registers the collector follows.** Every instruction that can
//!   collect carries a map ([`meadow_bytecode::GcMap`]) of the registers the
//!   program still names there and what each holds -- a reference, a scalar,
//!   or whatever the descriptor in another register says -- and the collector
//!   roots exactly those. `MEADOW_GC_VERIFY` overwrites every register the map
//!   leaves out, so a map that forgets something fails loudly instead of
//!   rarely.
//!
//! Heap objects say what their fields are in their headers -- see
//! [`crate::object`].
//!
//! The rule the whole file obeys: **never hold an address across an
//! allocation.** [`Vm::ensure`] is called first, with room for everything the
//! operation will build, and arguments are read out of registers afterwards.
//! Registers are roots; Rust locals are not.

use crate::heap::{Heap, Kind};
use crate::value::{Addr, Value, Word};
use meadow_bytecode::{Cond, Const, DESC_REG, DescSrc, Held, Instr, NO_MAP, Op, Pc, Program, Reg};
use meadow_intern::InternedString;
use meadow_rt::desc::{self, Desc};

/// How many registers there are. The compiler refuses to emit a block needing
/// more.
pub const REGISTERS: usize = 256;

/// Where the scratch area begins. The VM keeps a few slots the program cannot
/// name, so that rebuilding the register file on a call can go through them
/// instead of through a heap-allocated `Vec` — it was one malloc per call.
/// Nothing allocates while they are in use, so the collector never sees them.
pub(crate) const SCRATCH: usize = REGISTERS;
const SCRATCH_LEN: usize = 256;

/// The whole register file, as one fixed-size block of words. What a register
/// holds is the program's to say: see [`Vm::operand`].
pub(crate) type Regs = [Word; REGISTERS + SCRATCH_LEN];

/// The one register the compiler will not use, kept for a value the program
/// cannot name: the boolean a fused compare-and-branch tests and discards.
///
/// It is deliberately **not** a collector root — writing it does not raise
/// `live` — which is safe only because a fused branch's primitive is always a
/// comparison, and no comparison allocates. `meadow_codegen` will not emit one
/// for anything else ([`meadow_core::Prim::compares`]), and its register
/// allocator stops one short of the file so this slot stays free.
pub(crate) const TEMP: Reg = (REGISTERS - 1) as Reg;

/// [`Vm::at`] when no instruction has run: the machine is setting up a call,
/// and every register below `live` is a root.
pub(crate) const NO_PC: usize = usize::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub msg: String,
}

/// The condition a typed compare or branch names.
#[inline(always)]
fn cond(b: u32) -> Result<Cond, Error> {
    Cond::from_byte(b).ok_or_else(|| Error {
        msg: format!("no condition {b}"),
    })
}

pub(crate) fn err<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error { msg: msg.into() })
}

/// The machine.
///
/// `repr(C)`, and the fields native code reads and writes first, so that
/// where they are is fixed by this declaration and nothing else -- see
/// [`crate::codegen::layout`], which a test holds to it.
#[repr(C)]
pub struct Vm<'p> {
    /// The register file: 256 the program can name, then a scratch area it
    /// cannot. A boxed array rather than a `Vec` so its length is a constant —
    /// which is what lets the bounds check on every register access fold away.
    pub(crate) regs: Box<Regs>,
    pub(crate) live: usize,
    pub(crate) pc: usize,
    /// The instruction being carried out, or last carried out: whose map says
    /// what the registers hold if something collects. [`NO_PC`] before the
    /// first.
    pub(crate) at: usize,
    /// Instructions retired.
    pub steps: u64,
    /// [`crate::abi::meadow_exec`]: how native code has the interpreter carry
    /// out an instruction it does not do itself. Here rather than linked
    /// against, so native code needs no relocation to reach it.
    pub(crate) exec: crate::abi::ExecFn,
    /// Every method table's entry pcs, one after another, and where each table
    /// starts among them (with one more, where the last ends): how native code
    /// finds the method an `invoke` enters. Null until there is native code --
    /// see [`crate::jit::Native`].
    pub(crate) method_pcs: *const u32,
    pub(crate) method_starts: *const u32,
    /// The native function for each pc, or null: [`crate::jit::Native`]'s
    /// table, where native code looks to go straight on to the next block
    /// rather than return -- see `codegen`'s "Chaining". Null until there is
    /// native code.
    pub(crate) native_table: *const std::sync::atomic::AtomicPtr<std::ffi::c_void>,
    /// The native method entry for each pc that begins a method, or null:
    /// where native code's `invoke` goes with the arguments in registers, the
    /// entry loading the object's captures itself -- see `codegen`'s "Method
    /// entries". Null until there is native code.
    pub(crate) native_methods: *const std::sync::atomic::AtomicPtr<std::ffi::c_void>,
    pub(crate) heap: Heap,
    pub(crate) program: &'p Program,
    /// Values something outside the machine is holding on to — a debugger
    /// remembering which frame a step started in. Collector roots, so they are
    /// rewritten when what they point at moves. Empty unless someone asks.
    pub pinned: Vec<Value>,
    /// Where the program's console goes. `None` is the process's own stdin and
    /// stdout; a debugger speaking a protocol over those replaces both.
    pub io: Io,
    /// Who answers the effects this program leaves unhandled that are not
    /// the runtime's own, when another program is hosting this one -- see
    /// [`crate::host`]. `None` for a program run on its own.
    pub host: Option<crate::host::Host>,
    /// A thread operation the program just made, for the scheduler to carry
    /// out -- see [`crate::sched`]. The primitive leaves it here and the
    /// machine stops, since what happens next may be another thread's turn.
    pub(crate) request: Option<Request>,
    /// Is a scheduler running this machine? Without one there is nobody to
    /// carry out a request, and a thread operation is an error.
    pub(crate) scheduled: bool,
    /// Top-level values this thread has evaluated, by definition -- see
    /// `meadow_core::globals`. Collector roots. Each thread has its own, as it
    /// has its own heap.
    pub(crate) globals: Vec<Option<Value>>,
    /// The string each string literal made the first time it was loaded, by
    /// constant: a literal is immutable, so every load can answer the same
    /// one, and a loop comparing against `"let"` allocates nothing. Collector
    /// roots, like `globals`, and per thread for the same reason.
    pub(crate) literals: Vec<Option<Value>>,
    /// Every `TVar` of the run, when a scheduler is running this machine.
    pub(crate) world: Option<std::sync::Arc<crate::stm::World>>,
    /// The transaction this thread is in, if it is in one. Its values are in
    /// shared regions, not the heap, so nothing here is a collector root.
    pub(crate) txn: Option<crate::stm::Txn>,
    /// Native code for the program's blocks, by entry pc -- see [`crate::abi`].
    pub native: Option<&'p crate::jit::Native<'p>>,
    /// What native code stopped with: a halt's value, or a failure. Handed
    /// across the boundary here rather than through it.
    pub(crate) halted: Option<Value>,
    pub(crate) failure: Option<Error>,
    /// Samples, when someone asked for them. `None` costs one branch at each
    /// block the machine enters and nothing else -- see
    /// [`crate::profile`]. Last, because native code reaches the fields above
    /// by fixed offsets and must not be made to care about this one.
    pub profile: Option<Box<crate::profile::Profile>>,
    /// Which of the program this machine has reached, once the program has
    /// asked (`Process.covered`): a bit for each pc it has been entered at,
    /// and how many are set. `None` costs one branch where a sample's does;
    /// `Some` costs the machine its native code, from then on.
    /// What a fuzzer steers by: an input that reaches a pc no input before it
    /// did is one worth making more of. After `profile`, for its reason.
    pub(crate) reached: Option<Box<Reached>>,
    /// How many of each instruction this machine retired. See
    /// [`crate::profile::Ops`].
    #[cfg(feature = "profile-alloc")]
    pub ops: crate::profile::Ops,
    /// Where each `invoke` went, and which way each tag test did. See
    /// [`crate::profile::Feedback`].
    #[cfg(feature = "profile-alloc")]
    pub feedback: crate::profile::Feedback,
}

/// The pcs a machine has run an instruction at. See [`Vm::reached`].
///
/// A machine that keeps this is interpreted throughout ([`Vm::advance`]), so
/// every instruction it runs is noted: the count only grows, and grows
/// exactly when the program gets somewhere it had not been. Trusting native
/// code to come back at each block was tried first, and lost what a compiled
/// function reached inside itself: a fuzzer steered by it found its way only
/// while the function under test was still cold.
#[derive(Default)]
pub(crate) struct Reached {
    seen: Vec<u64>,
    count: u64,
}

impl Reached {
    #[inline]
    fn note(&mut self, pc: usize) {
        let (word, bit) = (pc / 64, 1u64 << (pc % 64));
        if word >= self.seen.len() {
            self.seen.resize(word + 1, 0);
        }
        if self.seen[word] & bit == 0 {
            self.seen[word] |= bit;
            self.count += 1;
        }
    }
}

impl Vm<'_> {
    /// How many pcs this machine has been entered at since the program first
    /// asked, which is when it starts to be noted -- or, `afresh`, since now:
    /// what was noted is forgotten, and the answer is nothing.
    pub(crate) fn covered(&mut self, afresh: bool) -> u64 {
        if afresh {
            self.reached = Some(Default::default());
        }
        self.reached.get_or_insert_with(Default::default).count
    }
}

// The method table pointers point into the `Native` the machine was given,
// which is borrowed for `'p`, and so outlives the machine.
unsafe impl Send for Vm<'_> {}

/// What a thread operation asks of the scheduler. `dst` is the register its
/// answer goes in, once there is one.
#[derive(Debug)]
pub(crate) enum Request {
    Spawn {
        body: crate::heap::Parcel,
        /// The descriptor of what the thread answers.
        answer: Desc,
        dst: Reg,
    },
    Await {
        task: u32,
        dst: Reg,
    },
    Yield,
    NewChannel {
        dst: Reg,
    },
    Send {
        channel: u32,
        message: crate::heap::Parcel,
    },
    Receive {
        channel: u32,
        dst: Reg,
    },
    /// The thread committed a transaction that wrote `written` while other
    /// threads were waiting on a `TVar`: wake the ones waiting on those, and
    /// `true` into `dst`. The commit itself is the thread's own business --
    /// see `Prim::StmCommit`.
    StmWake {
        written: Vec<u32>,
        dst: Reg,
    },
    /// Wait until something the thread's transaction read is written.
    StmWait {
        dst: Reg,
    },
}

/// Replacements for the console — see [`Vm::io`]. `Send`, since a green
/// thread moves between OS threads.
#[derive(Default)]
pub struct Io {
    /// Receives what `Console.writeOutput` writes when no handler takes it --
    /// which is where `print` and `println` end up.
    pub output: Option<Box<dyn FnMut(&str) + Send>>,
    /// Answers `Console.readLine`: a line without its terminator, or `None` at
    /// the end of input.
    pub input: Option<Box<dyn FnMut() -> Option<String> + Send>>,
}

/// Run `program` from its entry point and render the result the way the CEK
/// machine would.
///
/// The rendering rather than the value, because a [`Value`] means nothing away
/// from the heap it indexes — and because printing is what every caller wanted.
pub fn run(program: &Program, fuel: u64) -> Result<String, Error> {
    let Some(entry) = program.entry else {
        return err("program has no entry point");
    };
    crate::sched::run(program, entry, fuel).result
}

impl<'p> Vm<'p> {
    pub fn new(program: &'p Program) -> Vm<'p> {
        Vm::with_heap(program, Heap::new())
    }

    /// A machine whose heap is `heap` -- one made to collect a particular way.
    pub fn with_heap(program: &'p Program, heap: Heap) -> Vm<'p> {
        Vm {
            program,
            heap,
            regs: Box::new([0; REGISTERS + SCRATCH_LEN]),
            live: 0,
            pc: 0,
            at: NO_PC,
            steps: 0,
            exec: crate::abi::meadow_exec,
            method_pcs: std::ptr::null(),
            method_starts: std::ptr::null(),
            native_table: std::ptr::null(),
            native_methods: std::ptr::null(),
            pinned: Vec::new(),
            io: Io::default(),
            host: None,
            request: None,
            scheduled: false,
            globals: Vec::new(),
            literals: Vec::new(),
            world: None,
            txn: None,
            native: None,
            halted: None,
            failure: None,
            profile: None,
            reached: None,
            #[cfg(feature = "profile-alloc")]
            ops: crate::profile::Ops::default(),
            #[cfg(feature = "profile-alloc")]
            feedback: crate::profile::Feedback::default(),
        }
    }

    /// Run native code where `native` has some, from now on.
    pub fn use_native(&mut self, native: Option<&'p crate::jit::Native<'p>>) {
        self.native = native;
        match native {
            Some(n) => {
                self.method_pcs = n.method_pcs.as_ptr();
                self.method_starts = n.method_starts.as_ptr();
                self.native_table = n.table.as_ptr();
                self.native_methods = n.methods.as_ptr();
            }
            None => {
                self.method_pcs = std::ptr::null();
                self.method_starts = std::ptr::null();
                self.native_table = std::ptr::null();
                self.native_methods = std::ptr::null();
            }
        }
    }

    /// Put the machine at the start of a call: `f ()`, answering the halt
    /// continuation. How a green thread begins -- `f` is the function it was
    /// spawned with, rebuilt from `body` in this machine's own heap.
    ///
    /// `answer` is the descriptor of what `f` answers, which the halt it
    /// answers needs.
    pub(crate) fn start_call(
        &mut self,
        body: &crate::heap::Parcel,
        answer: Desc,
    ) -> Result<(), Error> {
        self.live = 0;
        self.heap
            .reserve(Heap::size_of(Kind::Closure, 1) + Heap::size_of(Kind::Data, 0) + body.len());
        let halt = Value::Obj(self.heap.alloc(Kind::Closure, 0, &[Value::Int(answer)]));
        let none = match self
            .program
            .ctors
            .iter()
            .position(|c| &**c == meadow_rt::roles::evidence::NONE)
        {
            Some(tag) => Value::Obj(self.heap.alloc(Kind::Data, tag as u32, &[])),
            None => return err("the program has no empty evidence to start a thread with"),
        };
        let f = self.heap.import(body);
        let Some(a) = f.addr().filter(|a| self.heap.kind(*a) == Kind::Closure) else {
            return err(format!(
                "a thread was started with {}, not a function",
                f.kind()
            ));
        };
        let table = self.heap.meta(a) as usize;
        let Some(&pc) = self.program.methods.get(table).and_then(|t| t.first()) else {
            return err("a thread's function has no method");
        };
        let ncap = self.heap.len(a);
        if ncap + 3 > REGISTERS {
            return err("a thread's function needs more than 256 registers");
        }
        for j in 0..ncap {
            self.regs[j] = self.heap.field_word(a, j);
        }
        // `f ()`, answering the halt continuation, under no handlers: a
        // function takes the evidence as its last argument.
        self.regs[ncap] = Value::Unit.bits();
        self.regs[ncap + 1] = halt.bits();
        self.regs[ncap + 2] = none.bits();
        self.live = ncap + 3;
        self.pc = pc as usize;
        self.at = NO_PC;
        Ok(())
    }

    /// Put `v`, rebuilt from `parcel`, in register `dst`: a thread operation's
    /// answer arriving.
    pub(crate) fn deliver_parcel(&mut self, dst: Reg, parcel: &crate::heap::Parcel) {
        self.ensure(parcel.len());
        let v = self.heap.import(parcel);
        self.set(dst, v);
    }

    /// A new handle object -- a channel or a thread -- in register `dst`.
    pub(crate) fn deliver_handle(&mut self, dst: Reg, kind: Kind, id: u32) {
        self.ensure(Heap::size_of(kind, 0));
        let a = self.heap.alloc(kind, id, &[]);
        self.set(dst, Value::Obj(a));
    }

    /// Run from `entry` until the program halts or `fuel` instructions retire.
    ///
    /// Bounded rather than open-ended because this is what differential tests
    /// call: a code generation bug that loops should fail a test, not hang it.
    pub fn run(&mut self, entry: Pc, fuel: u64) -> Result<Value, Error> {
        self.start(entry);
        // Saturating: `u64::MAX` is how a caller says "no limit", and adding it
        // to a machine that has already run would otherwise overflow.
        let limit = self.steps.saturating_add(fuel);
        loop {
            if self.steps >= limit {
                return err(format!("ran for {fuel} instructions without finishing"));
            }
            if let Some(v) = self.advance()? {
                return Ok(v);
            }
        }
    }

    /// Put the machine at `entry` with the halt continuation in `r0`.
    ///
    /// Method table 0 holds instruction 0, the `halt`. An entry block takes one
    /// parameter — the continuation to answer with — so returning from `main`
    /// needs no special case: it is an ordinary invoke that lands there. The
    /// continuation captures the descriptor of what `entry` answers.
    pub fn start(&mut self, entry: Pc) {
        self.heap.reserve(Heap::size_of(Kind::Closure, 1));
        let answer = self.program.result_at(entry);
        let halt = self.heap.alloc(Kind::Closure, 0, &[Value::Int(answer)]);
        self.regs[0] = Value::Obj(halt).bits();
        self.live = 1;
        self.pc = entry as usize;
        self.at = NO_PC;
    }

    /// One instruction. `Some` means the program halted.
    pub fn step(&mut self) -> Result<Option<Value>, Error> {
        let Some(&i) = self.program.code.get(self.pc) else {
            return err(format!("pc {} is outside the program", self.pc));
        };
        self.at = self.pc;
        #[cfg(feature = "profile-alloc")]
        {
            self.ops.note(i.op);
            // So the heap can charge what it bumps to the instruction that
            // asked for it. One store per instruction, and only when someone
            // asked for an allocation profile.
            self.heap.at = self.pc as meadow_bytecode::Pc;
        }
        self.pc += 1;
        self.steps += 1;
        self.exec(i)
    }

    /// Move the machine on: through native code for the block at the pc, if
    /// there is some -- which runs until control leaves the block -- or one
    /// instruction if not. `Some` means the program halted.
    #[inline]
    pub fn advance(&mut self) -> Result<Option<Value>, Error> {
        if self.profile.is_some() {
            self.sample();
        }
        // Noting where it goes, a machine is interpreted: native code runs
        // from one block into the next without coming back here, and what it
        // got to on the way would not be seen.
        if let Some(r) = &mut self.reached {
            r.note(self.pc);
            return self.step();
        }
        match self.native.and_then(|n| n.at(self.pc)) {
            Some(f) => crate::abi::enter(self, f),
            None => self.step(),
        }
    }

    /// Take a sample, if this entry is one.
    ///
    /// Split from the walk so that the borrow of the profile ends before the
    /// machine is read: the walk wants all of `self`.
    fn sample(&mut self) {
        let depth = match &mut self.profile {
            Some(p) => {
                if !p.due() {
                    return;
                }
                p.depth()
            }
            None => return,
        };
        let stack = self.stack(depth);
        if let Some(p) = &mut self.profile {
            p.record(stack);
        }
    }

    /// The pcs on the machine's stack, innermost first.
    ///
    /// There is no stack pointer to unwind from; there is a chain of
    /// continuations -- frames, and now and then a heap closure -- and walking it is
    /// the same walk. See [`crate::profile`] for why each link's method-0 entry
    /// pc is the frame below.
    ///
    /// Answers one frame -- where the machine is -- when the program carries no
    /// debug info, or when the chain runs into something that is not a closure,
    /// which is how it ends: the bottom of every chain is the `halt`.
    pub fn stack(&self, depth: usize) -> Vec<meadow_bytecode::Pc> {
        let mut out = Vec::with_capacity(8);
        out.push(self.pc as meadow_bytecode::Pc);
        let Some(debug) = self.program.debug.as_deref() else {
            return out;
        };
        let Some(reg) = crate::profile::continuation_reg(debug, self.pc) else {
            return out;
        };
        let mut at = self.reg(reg) as crate::value::Addr;

        while out.len() < depth {
            if !self.heap.holds_object(at)
                || !matches!(self.heap.kind(at), Kind::Closure | Kind::Frame)
            {
                break;
            }
            let entry = if self.heap.kind(at) == Kind::Frame {
                self.heap.meta(at)
            } else {
                let table = self.heap.meta(at) as usize;
                let Some(&entry) = self
                    .program
                    .methods
                    .get(table)
                    .and_then(|methods| methods.first())
                else {
                    break;
                };
                entry
            };
            out.push(entry);
            // The link below: the capture that is *that* function's return
            // continuation. A closure's captures are its method's first
            // parameters, in order, so the register a name is in at the
            // method's entry is the field it was captured into.
            let captures = self.heap.len(at);
            let Some(next) = crate::profile::continuation_reg(debug, entry as usize) else {
                break;
            };
            if next as usize >= captures {
                break;
            }
            at = self.heap.field_word(at, next as usize) as crate::value::Addr;
        }
        out
    }

    /// Carry out `i`, the instruction before the pc -- which a jump moves.
    pub(crate) fn exec(&mut self, i: Instr) -> Result<Option<Value>, Error> {
        match i.op {
            Op::Nop => {}

            Op::Move => {
                #[cfg(feature = "profile-alloc")]
                {
                    let code = &self.program.code;
                    let mut at = self.pc;
                    while code.get(at).is_some_and(|i| i.op == Op::Move) {
                        at += 1;
                    }
                    let op = code.get(at).map_or(Op::Nop, |i| i.op);
                    *self.feedback.moves.entry(op as u8).or_default() += 1;
                }
                let w = self.reg(i.b);
                self.set_word(i.a, w);
            }

            Op::Const => {
                let v = self.constant(i.imm)?;
                self.set(i.a, v);
            }

            Op::Jump => {
                self.pc = i.imm as usize;
                self.live = i.a as usize;
            }

            Op::JumpUnless => {
                if self.reg(i.a) == 0 {
                    self.pc = i.imm as usize;
                }
            }

            Op::JumpUnlessTag => {
                let want = i.bc() as u32;
                let a = self.reg(i.a) as Addr;
                let matched = self.heap.kind(a) == Kind::Data && self.heap.meta(a) == want;
                #[cfg(feature = "profile-alloc")]
                self.feedback.tag(self.at as Pc, matched);
                if !matched {
                    self.pc = i.imm as usize;
                }
            }

            Op::Halt => {
                let d = self.operand(0);
                if d == desc::ANY {
                    return err("the program answered with a value nothing describes");
                }
                return Ok(Some(Value::from_bits(self.reg(i.a), d)));
            }

            Op::Error => {
                let msg = self
                    .program
                    .messages
                    .get(i.imm as usize)
                    .cloned()
                    .unwrap_or_else(|| format!("error {}", i.imm));
                return err(msg);
            }

            Op::MakeData => self.make(i, Kind::Data, i.imm),
            Op::Blank => {
                let n = (usize::from(i.b) << 8) | usize::from(i.c);
                self.ensure(Heap::size_of(Kind::Data, n));
                let unit = Value::Unit.bits();
                let a = self
                    .heap
                    .alloc_described(Kind::Data, i.imm, n, |_| unit, |_| desc::UNIT);
                self.set(i.a, Value::Obj(a));
            }
            Op::Frame => self.frame(i),
            Op::MakeArray => self.make(i, Kind::Array, 0),

            Op::MakeRecord => {
                let n = i.c as usize;
                let Some(shape) = self.program.shapes.get(i.imm as usize) else {
                    return err(format!("no record shape {}", i.imm));
                };
                if shape.len() != n {
                    return err(format!(
                        "record shape has {} fields but {n} values were given",
                        shape.len()
                    ));
                }
                self.ensure(Heap::size_of(Kind::Record, 2 * n));
                // Sorted by label, so two records with the same fields are the
                // same object however they were written.
                let mut pairs: Vec<(InternedString, Value)> = self.program.shapes[i.imm as usize]
                    .iter()
                    .copied()
                    .zip((0..n).map(|j| self.value(i.b + j as Reg, j)))
                    .collect();
                pairs.sort_by_key(|(l, _)| *l);
                let mut fields = Vec::with_capacity(2 * n);
                for (l, v) in pairs {
                    fields.push(Value::Str(l));
                    fields.push(v);
                }
                let a = self.heap.alloc(Kind::Record, 0, &fields);
                self.set(i.a, Value::Obj(a));
            }

            Op::Field => {
                let a = self.reg(i.b) as Addr;
                let n = match self.heap.kind(a) {
                    Kind::Data => self.heap.len(a),
                    Kind::Array | Kind::Bytes => self.heap.array_len(a),
                    other => return err(format!("took field {} of a {other:?}", i.imm)),
                };
                if (i.imm as usize) >= n {
                    return err(format!("field {} of an object with {n}", i.imm));
                }
                let f = match self.heap.kind(a) {
                    Kind::Data => self.heap.field_word(a, i.imm as usize),
                    _ => self.heap.array_word(a, i.imm as usize),
                };
                self.set_word(i.a, f);
            }

            Op::Select => {
                let label = self.label(i.imm)?;
                let a = self.reg(i.b) as Addr;
                match self.heap.kind(a) {
                    Kind::Record => match self.record_get(a, label) {
                        Some(v) => self.set(i.a, v),
                        None => return err(format!("no field `{label}` on this record")),
                    },
                    // A `record` declaration's value: constructor data whose
                    // fields have names.
                    Kind::Data => {
                        let ctor = self.program.ctor(self.heap.meta(a));
                        let at = ctor
                            .and_then(|c| self.program.ctor_fields.get(&c))
                            .and_then(|fs| fs.iter().position(|f| *f == label))
                            .filter(|i| *i < self.heap.len(a));
                        match at {
                            Some(j) => {
                                let f = self.heap.field(a, j);
                                self.set(i.a, f);
                            }
                            None => {
                                let name = ctor.map_or("?".to_string(), |c| {
                                    meadow_rt::ctor_spelling(&c).to_string()
                                });
                                return err(format!("`{name}` has no field `{label}`"));
                            }
                        }
                    }
                    other => return err(format!("selected `.{label}` from a {other:?}")),
                }
            }

            Op::Extend => {
                let label = self.label(i.imm)?;
                let a = self.reg(i.b) as Addr;
                if self.heap.kind(a) != Kind::Record {
                    return err(format!(
                        "extended a {:?} with `.{label}`",
                        self.heap.kind(a)
                    ));
                }
                // Measure, make room, then re-read: the collection `ensure` may
                // run would move the record we are about to copy.
                let existing = self.heap.len(a) / 2;
                self.ensure(Heap::size_of(Kind::Record, 2 * (existing + 1)));
                let a = self.reg(i.b) as Addr;
                let value = self.value(i.c, 1);

                let mut pairs: Vec<(InternedString, Value)> = Vec::with_capacity(existing + 1);
                for j in 0..self.heap.len(a) / 2 {
                    match self.heap.field(a, 2 * j) {
                        Value::Str(l) => pairs.push((l, self.heap.field(a, 2 * j + 1))),
                        other => return err(format!("record label is {}", other.kind())),
                    }
                }
                match pairs.iter_mut().find(|(l, _)| *l == label) {
                    Some(slot) => slot.1 = value,
                    None => pairs.push((label, value)),
                }
                pairs.sort_by_key(|(l, _)| *l);
                let mut fields = Vec::with_capacity(pairs.len() * 2);
                for (l, v) in pairs {
                    fields.push(Value::Str(l));
                    fields.push(v);
                }
                let out = self.heap.alloc(Kind::Record, 0, &fields);
                self.set(i.a, Value::Obj(out));
            }

            Op::Closure => self.make(i, Kind::Closure, i.imm),

            Op::Invoke => return self.invoke(i),

            // Three shapes of the same thing: one and two arguments name their
            // registers, three come from a window.
            Op::Prim | Op::Prim1 | Op::Prim2 => {
                let Some(&p) = self.program.prims.get(i.imm as usize) else {
                    return err(format!("no primitive {}", i.imm));
                };
                let srcs = match i.op {
                    Op::Prim1 => [i.b, 0, 0],
                    Op::Prim2 => [i.b, i.c, 0],
                    _ => [i.b, i.b.wrapping_add(1), i.b.wrapping_add(2)],
                };
                self.run_prim(p, srcs, i.a)?;
            }

            // The right operand is a constant, and it goes into the destination
            // register before the primitive runs: `run_prim` re-reads its
            // arguments out of registers after any collection, and writes the
            // result last, so the destination is exactly the right place to
            // park it. The compiler never gives a folded primitive a
            // destination that is also its left operand.
            Op::PrimK => {
                debug_assert_ne!(i.a, i.b, "a folded primitive overwrote its own operand");
                let p = self.primitive(i.c as u32)?;
                let v = self.constant(i.imm)?;
                self.set(i.a, v);
                self.run_prim(p, [i.b, i.a, 0], i.a)?;
            }

            // A comparison and the branch that tests it. The boolean goes
            // nowhere the program can see, so it needs no register of its own.
            Op::JumpUnlessPrim | Op::JumpUnlessPrimK => {
                let p = self.primitive(i.c as u32)?;
                // The boolean — and a folded constant — go in [`TEMP`], which
                // the program cannot name. Writing any register raises `live`,
                // and leaving it raised would make the whole file a collector
                // root until the next jump, so it is put back. Nothing can
                // observe the gap: a fused branch's primitive is a comparison,
                // and no comparison allocates.
                let live = self.live;
                let srcs = if i.op == Op::JumpUnlessPrimK {
                    let v = self.constant(i.b as u32)?;
                    self.set(TEMP, v);
                    [i.a, TEMP, 0]
                } else {
                    [i.a, i.b, 0]
                };
                self.run_prim(p, srcs, TEMP)?;
                let cond = self.reg(TEMP);
                self.live = live;
                if cond == 0 {
                    self.pc = i.imm as usize;
                }
            }

            Op::Native => self.native_op(i)?,

            // --- typed: arithmetic on words ------------------------------
            Op::AddI => self.int2(i, i64::wrapping_add),
            Op::SubI => self.int2(i, i64::wrapping_sub),
            Op::MulI => self.int2(i, i64::wrapping_mul),
            Op::DivI | Op::ModI => {
                let (x, y) = (self.reg(i.b) as i64, self.reg(i.c) as i64);
                if y == 0 {
                    return err(if i.op == Op::DivI {
                        "division by zero"
                    } else {
                        "modulo by zero"
                    });
                }
                let r = if i.op == Op::DivI {
                    x.wrapping_div(y)
                } else {
                    x.wrapping_rem(y)
                };
                self.set_word(i.a, r as Word);
            }
            // `wrapping_shl` takes the count modulo 64, as the hardware
            // does, so the typed instruction and `num::int_bits` agree.
            Op::ShlI => self.int2(i, |x, y| x.wrapping_shl(y as u32)),
            Op::ShrI => self.int2(i, |x, y| x.wrapping_shr(y as u32)),
            Op::AndI => self.int2(i, |x, y| x & y),
            Op::ShlIK => self.int_k(i, |x, y| x.wrapping_shl(y as u32)),
            Op::ShrIK => self.int_k(i, |x, y| x.wrapping_shr(y as u32)),
            Op::AndIK => self.int_k(i, |x, y| x & y),
            Op::UshrI => self.int2(i, |x, y| ((x as u64).wrapping_shr(y as u32)) as i64),
            Op::UshrIK => self.int_k(i, |x, y| ((x as u64).wrapping_shr(y as u32)) as i64),
            Op::PopI => {
                let n = self.reg(i.b).count_ones() as Word;
                self.set_word(i.a, n);
            }
            Op::ItoF => {
                let x = self.reg(i.b) as i64 as f64;
                self.set_word(i.a, x.to_bits());
            }
            Op::AddIK => self.int_k(i, i64::wrapping_add),
            Op::SubIK => self.int_k(i, i64::wrapping_sub),
            Op::MulIK => self.int_k(i, i64::wrapping_mul),
            Op::AddF => self.float2(i, |x, y| x + y),
            Op::SubF => self.float2(i, |x, y| x - y),
            Op::MulF => self.float2(i, |x, y| x * y),
            Op::DivF => self.float2(i, |x, y| x / y),
            Op::CmpI => {
                let holds = cond(i.imm)?.words(self.reg(i.b), self.reg(i.c));
                self.set_word(i.a, holds as Word);
            }
            Op::CmpIK => {
                let k = i.imm as i32 as i64 as Word;
                let holds = cond(i.c as u32)?.words(self.reg(i.b), k);
                self.set_word(i.a, holds as Word);
            }
            Op::CmpF => {
                let (x, y) = (f64::from_bits(self.reg(i.b)), f64::from_bits(self.reg(i.c)));
                let holds = cond(i.imm)?.floats(x, y);
                self.set_word(i.a, holds as Word);
            }
            Op::BrI => {
                if !cond(i.c as u32)?.words(self.reg(i.a), self.reg(i.b)) {
                    self.pc = i.imm as usize;
                }
            }
            Op::BrIK => {
                let k = i.b as i8 as i64 as Word;
                if !cond(i.c as u32)?.words(self.reg(i.a), k) {
                    self.pc = i.imm as usize;
                }
            }
            Op::BrF => {
                let (x, y) = (f64::from_bits(self.reg(i.a)), f64::from_bits(self.reg(i.b)));
                if !cond(i.c as u32)?.floats(x, y) {
                    self.pc = i.imm as usize;
                }
            }
        }
        Ok(None)
    }

    #[inline(always)]
    fn int2(&mut self, i: Instr, f: fn(i64, i64) -> i64) {
        let r = f(self.reg(i.b) as i64, self.reg(i.c) as i64);
        self.set_word(i.a, r as Word);
    }

    #[inline(always)]
    fn int_k(&mut self, i: Instr, f: fn(i64, i64) -> i64) {
        let r = f(self.reg(i.b) as i64, i.imm as i32 as i64);
        self.set_word(i.a, r as Word);
    }

    #[inline(always)]
    fn float2(&mut self, i: Instr, f: fn(f64, f64) -> f64) {
        let r = f(f64::from_bits(self.reg(i.b)), f64::from_bits(self.reg(i.c)));
        self.set_word(i.a, r.to_bits());
    }

    // --- control ----------------------------------------------------------

    /// Enter a method of the object in `r[a]`, or resume a continuation.
    ///
    /// The one calling convention: the register file becomes the object's
    /// captures followed by the arguments, and the pc becomes the method's
    /// entry. Nothing is saved, because the only way back is a continuation the
    /// caller already passed as one of those arguments.
    fn invoke(&mut self, i: Instr) -> Result<Option<Value>, Error> {
        let a = self.reg(i.a) as Addr;
        let base = i.c;
        let argc = i.imm as usize;

        match self.heap.kind(a) {
            kind @ (Kind::Closure | Kind::Frame) => {
                let ncap = self.heap.len(a);
                if ncap + argc > REGISTERS {
                    return err("a call needs more than 256 registers");
                }
                let pc = if kind == Kind::Frame {
                    self.heap.meta(a)
                } else {
                    let table = self.heap.meta(a) as usize;
                    let Some(&pc) = self
                        .program
                        .methods
                        .get(table)
                        .and_then(|t| t.get(i.b as usize))
                    else {
                        return err(format!("no method #{} on this object", i.b));
                    };
                    pc
                };
                #[cfg(feature = "profile-alloc")]
                self.feedback.call(self.at as Pc, pc, kind == Kind::Frame);
                // What the JIT will guess this site enters, once its block is
                // hot: see `jit::Native::observe`.
                if let Some(n) = self.native {
                    let frame = kind == Kind::Frame;
                    let meta = if frame { pc } else { self.heap.meta(a) };
                    n.observe(self.at, crate::codegen::Known { frame, meta });
                }
                // Arguments out of the way first: writing the captures into
                // r0.. would otherwise clobber the window they sit in. Through
                // the scratch area rather than a `Vec`, because this is the
                // calling convention and it ran a heap allocation per call.
                let base = base as usize;
                self.regs.copy_within(base..base + argc, SCRATCH);
                for j in 0..ncap {
                    self.regs[j] = self.heap.field_word(a, j);
                }
                self.regs.copy_within(SCRATCH..SCRATCH + argc, ncap);
                self.live = ncap + argc;
                self.pc = pc as usize;
                // A frame is a function's return: it is done with, and so is
                // everything the function pushed above it.
                if kind == Kind::Frame {
                    self.heap.pop_to(a);
                }
                Ok(None)
            }

            other => err(format!("invoked a {other:?}")),
        }
    }

    /// An effect operation no handler in the program answers: `Fs`,
    /// `Process`, `Random`, `Time` and `Console` reach the real world (see
    /// [`crate::native`]), and `Test.fail` is a failed assertion, which is a
    /// runtime error carrying the assertion's own message for a test runner to
    /// read.
    fn native_op(&mut self, i: Instr) -> Result<(), Error> {
        let Some(&(effect, op)) = self.program.ops.get(i.imm as usize) else {
            return err(format!("no operation {}", i.imm));
        };
        let arg = self.value(i.b, 0);
        if &*effect == "Test" && &*op == "fail" {
            return err(self.show(arg));
        }
        match self.native(&effect, &op, arg)? {
            Some(v) => {
                self.set(i.a, v);
                Ok(())
            }
            None => {
                let effect = meadow_rt::spelling(&effect);
                err(format!("unhandled effect {effect}.{op}"))
            }
        }
    }

    // --- registers and the heap -------------------------------------------

    #[inline]
    pub(crate) fn reg(&self, r: Reg) -> Word {
        self.regs[r as usize]
    }

    #[inline]
    pub(crate) fn set(&mut self, r: Reg, v: Value) {
        self.set_word(r, v.bits());
    }

    #[inline]
    pub(crate) fn set_word(&mut self, r: Reg, w: Word) {
        self.regs[r as usize] = w;
        self.live = self.live.max(r as usize + 1);
    }

    /// The descriptor of operand `k` of the instruction being carried out:
    /// the compiler's, or the one it says a register holds.
    #[inline]
    pub(crate) fn operand(&self, k: usize) -> Desc {
        let at = match self.program.operands_at.get(self.at) {
            Some(&at) if at != meadow_bytecode::NO_OPERANDS => at as usize + k,
            _ => return desc::ANY,
        };
        match self.program.operands.get(at) {
            Some(&src) => self.desc_at(src),
            None => desc::ANY,
        }
    }

    #[inline]
    fn desc_at(&self, src: DescSrc) -> Desc {
        if src < DESC_REG {
            src as Desc
        } else {
            self.regs[(src - DESC_REG) as usize] as Desc
        }
    }

    /// The value in register `r`, which is operand `k` of the instruction being
    /// carried out.
    #[inline]
    pub(crate) fn value(&self, r: Reg, k: usize) -> Value {
        let d = self.operand(k);
        if d == desc::ANY {
            self.missing_descriptor(k, r);
        }
        Value::from_bits(self.reg(r), d)
    }

    /// The compiler did not say what operand `k`, in `r`, of the instruction
    /// being carried out holds: a bug in it, which is worth naming the
    /// instruction for.
    #[cold]
    pub(crate) fn missing_descriptor(&self, k: usize, r: Reg) -> ! {
        let at = self.at;
        let name = self
            .program
            .debug
            .as_ref()
            .and_then(|d| d.region(at as Pc))
            .map(|r| r.name.to_string())
            .unwrap_or_default();
        let listing = self.program.disassemble();
        let around: Vec<&str> = listing
            .lines()
            .filter(|l| {
                l.split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<usize>().ok())
                    .is_some_and(|pc| pc + 14 >= at && pc <= at + 3)
            })
            .collect();
        panic!(
            "operand {k} (r{r}) of {:?} at pc {at} {name} has no descriptor\n{}",
            self.program.code.get(at).map(|i| i.op),
            around.join("\n")
        );
    }

    /// `MakeData`, `MakeArray` and `Closure`: an object of the window's words,
    /// described by the instruction's operands.
    fn make(&mut self, i: Instr, kind: Kind, meta: u32) {
        let n = i.c as usize;
        self.ensure(Heap::size_of(kind, n));
        let fields = self.program.fields(self.at, i);
        let operands = self.program.operands(self.at);
        let a = {
            let Vm { heap, regs, .. } = self;
            let desc = |j: usize| {
                let src = operands[j];
                if src < DESC_REG {
                    src as Desc
                } else {
                    regs[(src - DESC_REG) as usize] as Desc
                }
            };
            // An array of bytes is kept a byte to an element.
            if kind == Kind::Array && n > 0 && (0..n).all(|j| desc(j) == Heap::BYTE) {
                let words: Vec<Word> = (0..n).map(|j| regs[fields.reg(j) as usize]).collect();
                heap.alloc_array(&words, Heap::BYTE)
            } else {
                heap.alloc_described(kind, meta, n, |j| regs[fields.reg(j) as usize], desc)
            }
        };
        self.set(i.a, Value::Obj(a));
    }

    /// A frame: what `make` would make of a closure, pushed on the frame
    /// stack instead. Nothing is allocated in the heap, so nothing can move.
    fn frame(&mut self, i: Instr) {
        let n = i.c as usize;
        let fields = self.program.fields(self.at, i);
        let operands = self.program.operands(self.at);
        // A frame's `meta` is where returning through it goes -- its one
        // method's pc -- rather than a method table: a return is a jump, with
        // no table to look the jump up in.
        let pc = self
            .program
            .methods
            .get(i.imm as usize)
            .and_then(|t| t.first())
            .copied()
            .unwrap_or(0);
        let a = {
            let Vm { heap, regs, .. } = self;
            let desc = |j: usize| {
                let src = operands[j];
                if src < DESC_REG {
                    src as Desc
                } else {
                    regs[(src - DESC_REG) as usize] as Desc
                }
            };
            heap.push_frame(pc, n, |j| regs[fields.reg(j) as usize], desc)
        };
        self.set(i.a, Value::Obj(a));
    }

    /// Make room for `slots`, collecting and growing as needed.
    ///
    /// Everything reachable must be in a register below `live` or on the
    /// handler stack when this is called — see the module docs.
    pub(crate) fn ensure(&mut self, slots: usize) {
        // Regions are freed only by collecting, so enough written to them asks
        // for a collection even while the nursery has room.
        if self.heap.room_for(slots) && !self.heap.wants_collection() {
            return;
        }
        self.collect();
        self.heap.reserve(slots);
    }

    fn collect(&mut self) {
        let registers = self.root_registers();
        let mut roots: Vec<Value> =
            Vec::with_capacity(registers.len() + self.pinned.len() + self.globals.len());
        roots.extend(registers.iter().map(|r| Value::Obj(self.regs[*r] as Addr)));
        roots.extend_from_slice(&self.pinned);
        roots.extend(self.globals.iter().flatten().copied());
        roots.extend(self.literals.iter().flatten().copied());

        if self.heap.overdue() {
            self.heap.collect_all(&mut roots);
        } else {
            self.heap.collect(&mut roots);
        }

        let mut it = roots.into_iter();
        for r in registers {
            self.regs[r] = it.next().expect("root count").bits();
        }
        for p in &mut self.pinned {
            *p = it.next().expect("root count");
        }
        for g in self.globals.iter_mut().flatten() {
            *g = it.next().expect("root count");
        }
        for l in self.literals.iter_mut().flatten() {
            *l = it.next().expect("root count");
        }
    }

    /// The registers that hold addresses now, by the map of the instruction
    /// being carried out.
    ///
    /// A register the map calls a reference is one; a scalar is not; one whose
    /// representation depends on a type variable is what its descriptor, in
    /// another register, says. Registers are words, so there is nothing else
    /// to go by: a collection where there is no map is a compiler bug.
    ///
    /// Verifying, the registers the map leaves out are zeroed, so a map missing
    /// a register the program reads again shows up as a wrong value rather than
    /// a rare dangling address.
    fn root_registers(&mut self) -> Vec<usize> {
        let map = self
            .program
            .gc_at
            .get(self.at)
            .copied()
            .filter(|m| *m != NO_MAP);
        let Some(map) = map else {
            if self.live == 0 {
                return Vec::new();
            }
            let op = self.program.code.get(self.at).map(|i| i.op);
            panic!(
                "a collection at pc {} ({op:?}), which has no register map",
                self.at
            );
        };
        let map = &self.program.gc_maps[map as usize];
        let verifying = self.heap.verifying();
        let mut roots = Vec::with_capacity(map.regs.len());
        let mut next = 0;
        for &(r, held) in &map.regs {
            let r = r as usize;
            if r >= self.live {
                if verifying {
                    panic!(
                        "pc {} maps r{r}, above the {} live registers",
                        self.at, self.live
                    );
                }
                continue;
            }
            if verifying {
                while next < r {
                    self.regs[next] = 0;
                    next += 1;
                }
                next = r + 1;
            }
            let pointer = match held {
                Held::Ref => true,
                Held::Scalar => false,
                Held::Var(d) => {
                    let code = self.regs[d as usize];
                    assert!(
                        code < 16,
                        "pc {} describes r{r} by r{d}, which holds {code}, not a descriptor",
                        self.at
                    );
                    assert_ne!(
                        code as Desc,
                        desc::ANY,
                        "pc {}: r{r} holds a value nothing describes",
                        self.at
                    );
                    code as Desc == desc::REF
                }
                Held::Any => panic!("pc {}: r{r} holds a value nothing describes", self.at),
            };
            if pointer {
                roots.push(r);
            }
        }
        if verifying {
            while next < self.live {
                self.regs[next] = 0;
                next += 1;
            }
        }
        roots
    }

    fn primitive(&self, id: u32) -> Result<meadow_rt::Prim, Error> {
        match self.program.prims.get(id as usize) {
            Some(p) => Ok(*p),
            None => err(format!("no primitive {id}")),
        }
    }

    fn constant(&mut self, id: u32) -> Result<Value, Error> {
        let Some(c) = self.program.consts.get(id as usize) else {
            return err(format!("no constant {id}"));
        };
        Ok(match *c {
            Const::Unit => Value::Unit,
            Const::Bool(b) => Value::Bool(b),
            Const::Int(n) => Value::Int(n),
            Const::Float(x) => Value::Float(x),
            Const::Word(w, b) => Value::Word(w, b),
            Const::Float32(x) => Value::Float32(x),
            Const::Str(s) => Value::Str(s),
            Const::Char(c) => Value::Char(c),
            Const::BigInt(n) => self.alloc_bigint(num_bigint::BigInt::from(n)),
            Const::Text(s) => {
                let at = id as usize;
                if let Some(Some(made)) = self.literals.get(at) {
                    return Ok(*made);
                }
                let made = self.new_text(s.as_bytes());
                if self.literals.len() <= at {
                    self.literals.resize(at + 1, None);
                }
                self.literals[at] = Some(made);
                made
            }
        })
    }

    fn label(&self, id: u32) -> Result<InternedString, Error> {
        match self.program.labels.get(id as usize) {
            Some(l) => Ok(*l),
            None => err(format!("no field label {id}")),
        }
    }

    pub(crate) fn record_get(&self, a: Addr, label: InternedString) -> Option<Value> {
        for j in 0..self.heap.len(a) / 2 {
            if self.heap.field(a, 2 * j) == Value::Str(label) {
                return Some(self.heap.field(a, 2 * j + 1));
            }
        }
        None
    }

    // --- introspection ----------------------------------------------------

    /// The live registers, named by number — what a stepper shows beside the
    /// disassembly.
    ///
    /// Registers are words; this shows each as the number it is.
    pub fn describe(&self) -> String {
        let mut out = String::from("[");
        for j in 0..self.live {
            if j > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("r{j} = {:#x}", self.regs[j]));
        }
        out.push(']');
        out
    }

    pub fn pc(&self) -> usize {
        self.pc
    }

    /// The image being run.
    pub fn program(&self) -> &'p Program {
        self.program
    }

    /// How many registers are live: `r0..live` is what the collector keeps.
    pub fn live(&self) -> usize {
        self.live
    }

    /// The word in register `r`. Only `r0..live` is meaningful — above that a
    /// register may hold an address the collector has since invalidated.
    pub fn register(&self, r: usize) -> Word {
        self.regs.get(r).copied().unwrap_or(0)
    }

    /// Register `r`'s word as a value of descriptor `d`.
    pub fn register_as(&self, r: usize, d: Desc) -> Value {
        Value::from_bits(self.register(r), d)
    }

    /// The heap, to look inside objects. Check [`Heap::is_object`] before
    /// reading an address that did not come from a live register.
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// Write program output, wherever it is going.
    pub(crate) fn write_out(&mut self, s: &str) {
        match &mut self.io.output {
            Some(out) => out(s),
            None => {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(s.as_bytes());
            }
        }
    }

    /// Collections so far and slots allocated in total.
    pub fn heap_stats(&self) -> (u64, u64) {
        (self.heap.collections, self.heap.allocated)
    }
}
