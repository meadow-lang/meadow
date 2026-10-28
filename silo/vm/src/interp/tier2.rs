//! **Tier 2 of the JIT**: a definition that runs often, lowered as an
//! executable's definitions are -- by `meadow_llvm`, to the LIR -- and
//! compiled by Cranelift, in the VM's process (`docs/RUNTIME-SPEC.md`,
//! JIT-3 and LIR-3).
//!
//! **What is compiled.** A definition, with what is written with it: its
//! methods, its frames and its stack trampoline. The lowering is the one an
//! executable is built by, in its JIT mode (`meadow_llvm::emit::Jit`), so its
//! code is an executable's: frames are the native stack, a tail call is a
//! tail call, a value is in a register, and what a layout and a memory
//! strategy decide is written out in the LIR as it is for LLVM. What differs
//! is how it is numbered: a closure's methods, a tag, an interned name and a
//! cache as the VM numbered them when it loaded the program, since what the
//! compiled code makes the interpreted code takes apart, and the other way.
//!
//! **Calls.** A call by name goes through a table by the definition's slot,
//! the one hot loading replaces (JIT-4): the compiled function where there is
//! one, and otherwise the VM. A method is the VM's to find, by its number
//! (`meadow_jit_method`): compiled, or the VM. When the callee is the VM's:
//! - a call that waits for its answer asks the VM for it, which runs the
//!   callee on this native stack, as an interpreted call would;
//! - a tail call cannot, or a loop between the two would take more stack
//!   at each turn. It leaves the call *pending* -- the procedure and its
//!   arguments, on this thread -- and answers [`SENTINEL`]. That travels
//!   back up the tail calls to whoever waits: compiled code that called and
//!   waits, which then asks the VM to run what is pending ([`jit_resume`]),
//!   or the VM's loop, which runs it as its next procedure.
//!
//! A real answer equal to the sentinel is told from it by there being
//! nothing pending.
//!
//! **Memory.** The code keeps the records the lowering writes -- a frame's
//! on the shadow chain, a function's at its safe point -- and reads back what
//! a collection moved, as an executable does; the VM's own records are made
//! by the VM around what it calls. Allocation is the runtime's.
//!
//! **Not compiled.** A definition whose lowering needs a number the VM does
//! not have -- a closure made by code the VM packed itself -- or an
//! instruction this translation does not know stays with tier 1.

use super::{Args, Env, LOCAL, SEG, V, Vm, Word};
use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    self, AbiParam, Block, InstBuilder, Signature, StackSlotData, StackSlotKind, Type, Value, types,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use meadow_lir::{BinOp, Cast, FCmp, Func, ICmp, Inst, Operand, Ty};
use smallvec::SmallVec;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, Ordering};

/// What compiled code answers when it has left a tail call to the VM: see
/// the module docs. A NaN's bits, which spell `meadow` in its payload.
pub(super) const SENTINEL: u64 = 0x7FF4_6D65_6164_6F77;

/// The second tier: what it has compiled, and what compiles.
pub(super) struct Tier2 {
    /// How many entries after tier 1's threshold make a definition worth
    /// compiling again, here.
    pub(super) threshold: u32,
    /// Whether compiled functions take the thread's context first, and how
    /// many words a call passes in registers, that among them.
    threaded: bool,
    regs: usize,
    inner: Mutex<Inner>,
    /// By definition slot: the compiled definition's function, in
    /// Cranelift's tail convention, or null.
    slots: Box<[AtomicPtr<u8>]>,
}

/// A segment's, beside tier 1's table: by procedure, a definition's compiled
/// function and how it is entered; by method, its compiled function; and how
/// many entries each procedure has had since tier 1 compiled it.
pub(super) struct SegTable {
    procs: Box<[AtomicPtr<u8>]>,
    /// The way in from Rust to each, for its arity.
    tramps: Box<[AtomicPtr<u8>]>,
    /// For each procedure: 0 not tried, 1 compiled, 2 refused.
    state: Box<[AtomicU8]>,
    entered: Box<[AtomicU32]>,
    methods: Box<[AtomicPtr<u8>]>,
}

impl SegTable {
    pub(super) fn new(procs: usize, methods: usize) -> SegTable {
        SegTable {
            procs: (0..procs).map(|_| AtomicPtr::default()).collect(),
            tramps: (0..procs).map(|_| AtomicPtr::default()).collect(),
            state: (0..procs).map(|_| AtomicU8::new(0)).collect(),
            entered: (0..procs).map(|_| AtomicU32::new(0)).collect(),
            methods: (0..methods).map(|_| AtomicPtr::default()).collect(),
        }
    }
}

struct Inner {
    module: JITModule,
    cl: Context,
    fctx: FunctionBuilderContext,
    /// Each segment's lowering, kept from one definition to the next: it
    /// numbers its functions' names as it goes.
    lowerings: HashMap<u32, Box<meadow_llvm::emit::Module<'static>>>,
    /// What is declared, by its name in the module (see [`mangled`]).
    funcs: HashMap<String, (FuncId, Signature)>,
    data: HashMap<String, DataId>,
    helpers: HashMap<u32, bool>,
    /// The ways in from Rust: by how many words in registers.
    trampolines: HashMap<usize, *const u8>,
    threaded: bool,
    regs: usize,
}

// Safety: the module's code and data are shared only once finalized, and
// everything else is used under the lock.
unsafe impl Send for Inner {}

/// The runtime's entry points and statics the emitted code names, by name:
/// the executable's linker finds them in an executable, and this in the VM.
fn runtime_symbols() -> Vec<(&'static str, *const u8)> {
    use meadow_silo as rt;
    vec![
        ("meadow_acquire", rt::meadow_acquire as *const u8),
        (
            "meadow_acquire_traced",
            rt::meadow_acquire_traced as *const u8,
        ),
        ("meadow_bigint", rt::meadow_bigint as *const u8),
        ("meadow_clean", rt::meadow_clean as *const u8),
        ("meadow_field", rt::meadow_field as *const u8),
        ("meadow_free", rt::meadow_free as *const u8),
        ("meadow_spill_area", rt::meadow_spill_area as *const u8),
        ("meadow_fail", rt::meadow_fail as *const u8),
        ("meadow_text", rt::meadow_text as *const u8),
        ("meadow_ctx", rt::ctx::meadow_ctx as *const u8),
        (
            "meadow_candidate",
            rt::cycles::meadow_candidate as *const u8,
        ),
        ("meadow_native", rt::native::meadow_native as *const u8),
        (
            "meadow_array_concat",
            rt::prims::meadow_array_concat as *const u8,
        ),
        (
            "meadow_array_push",
            rt::prims::meadow_array_push as *const u8,
        ),
        ("meadow_equal", rt::prims::meadow_equal as *const u8),
        ("meadow_extend", rt::prims::meadow_extend as *const u8),
        ("meadow_get_ref", rt::prims::meadow_get_ref as *const u8),
        (
            "meadow_global_get",
            rt::prims::meadow_global_get as *const u8,
        ),
        (
            "meadow_global_ready",
            rt::prims::meadow_global_ready as *const u8,
        ),
        ("meadow_hash", rt::prims::meadow_hash as *const u8),
        ("meadow_select", rt::prims::meadow_select as *const u8),
        ("meadow_st_set", rt::prims::meadow_st_set as *const u8),
        (
            "meadow_string_index_of",
            rt::prims::meadow_string_index_of as *const u8,
        ),
        (
            "meadow_string_slice",
            rt::prims::meadow_string_slice as *const u8,
        ),
        ("meadow_update", rt::prims::meadow_update as *const u8),
        ("meadow_array", rt::prims::meadow_array as *const u8),
        ("meadow_prim", rt::prims::meadow_prim as *const u8),
        ("meadow_record", rt::prims::meadow_record as *const u8),
        (
            "meadow_region_erased",
            rt::region::meadow_region_erased as *const u8,
        ),
        (
            "meadow_region_shared",
            rt::region::meadow_region_shared as *const u8,
        ),
        ("meadow_preempted", rt::sched::meadow_preempted as *const u8),
        ("meadow_detach", rt::segments::meadow_detach as *const u8),
        ("meadow_enter", rt::segments::meadow_enter as *const u8),
        (
            "meadow_reattach",
            rt::segments::meadow_reattach as *const u8,
        ),
        (
            "meadow_stack_grow",
            rt::stack::meadow_stack_grow as *const u8,
        ),
        ("meadow_gc_poll", rt::trace::meadow_gc_poll as *const u8),
        ("meadow_remember", rt::trace::meadow_remember as *const u8),
        ("meadow_jit_method", meadow_jit_method as *const u8),
        ("meadow_jit_pend_def", jit_pend_def as *const u8),
        ("meadow_jit_call_def", jit_call_def as *const u8),
        ("meadow_jit_pend_method", jit_pend_method as *const u8),
        ("meadow_jit_call_method", jit_call_method as *const u8),
        ("meadow_jit_resume", jit_resume as *const u8),
        // Statics.
        (
            "meadow_preempt",
            &rt::sched::meadow_preempt as *const _ as *const u8,
        ),
        (
            "meadow_gc_due",
            &rt::trace::meadow_gc_due as *const _ as *const u8,
        ),
        (
            "meadow_shadow_single",
            (&raw const rt::shadow::meadow_shadow_single) as *const u8,
        ),
        (
            "meadow_stack_limit_single",
            (&raw const rt::stack::meadow_stack_limit_single) as *const u8,
        ),
    ]
}

impl Tier2 {
    /// The second tier `MEADOW_JIT_TIERS` (2 by default) asks for, compiling
    /// what is entered `MEADOW_JIT_TIER2` (10000 by default) times more.
    pub(super) fn from_env(threaded: bool, regs: usize) -> Result<Option<Tier2>, String> {
        let tiers = std::env::var("MEADOW_JIT_TIERS").unwrap_or_else(|_| "2".into());
        if tiers.trim() == "1" {
            return Ok(None);
        }
        let threshold = match std::env::var("MEADOW_JIT_TIER2") {
            Ok(n) => n
                .trim()
                .parse()
                .map_err(|_| format!("MEADOW_JIT_TIER2 is `{n}`, not a number of entries"))?,
            Err(_) => 10_000,
        };
        let mut flags = settings::builder();
        flags.set("opt_level", "speed").map_err(|e| e.to_string())?;
        // What the code names may be further than a 32-bit displacement
        // reaches -- the JIT's memory is wherever the system gives it -- so
        // addresses are loaded, as cranelift-jit wants on x86-64.
        let pic = if cfg!(target_arch = "x86_64") {
            "true"
        } else {
            "false"
        };
        flags.set("is_pic", pic).map_err(|e| e.to_string())?;
        flags
            .set("use_colocated_libcalls", "false")
            .map_err(|e| e.to_string())?;
        // Cranelift's tail calls are made through the frame pointer.
        flags
            .set("preserve_frame_pointers", "true")
            .map_err(|e| e.to_string())?;
        let isa = cranelift_native::builder()
            .map_err(|e| e.to_string())?
            .finish(settings::Flags::new(flags))
            .map_err(|e| e.to_string())?;
        let mut b = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        for (name, at) in runtime_symbols() {
            b.symbol(name, at);
        }
        let module = JITModule::new(b);
        Ok(Some(Tier2 {
            threshold,
            threaded,
            regs,
            inner: Mutex::new(Inner {
                cl: module.make_context(),
                module,
                fctx: FunctionBuilderContext::new(),
                lowerings: HashMap::new(),
                funcs: HashMap::new(),
                data: HashMap::new(),
                helpers: HashMap::new(),
                trampolines: HashMap::new(),
                threaded,
                regs,
            }),
            slots: (0..super::DEFS).map(|_| AtomicPtr::default()).collect(),
        }))
    }

    /// A slot a reload gave a new version: what was compiled for the old one
    /// is no longer what a call by name runs.
    pub(super) fn forget_slot(&self, slot: u32) {
        self.slots[slot as usize].store(std::ptr::null_mut(), Ordering::Release);
    }
}

impl Vm {
    /// Procedure `p`'s compiled function, if it is a definition tier 2 has
    /// compiled -- compiling it, if it has been entered often enough -- with
    /// the way in from Rust for its arity.
    pub(super) fn tier2(&self, p: u32) -> Option<(*const u8, *const u8)> {
        let t2 = self.jit.as_ref()?.tier2.as_ref()?;
        let seg = self.seg(p);
        let local = (p & LOCAL) as usize;
        if local >= seg.lowered.blocks.len() {
            return None;
        }
        let table = &seg.jit.tier2;
        let f = table.procs[local].load(Ordering::Acquire);
        if f.is_null() {
            if table.state[local].load(Ordering::Relaxed) != 0 {
                return None;
            }
            let counter = &table.entered[local];
            let n = counter.load(Ordering::Relaxed);
            counter.store(n + 1, Ordering::Relaxed);
            if n < t2.threshold {
                return None;
            }
            self.tier2_compile(t2, p);
            let f = table.procs[local].load(Ordering::Acquire);
            if f.is_null() {
                return None;
            }
        }
        let f = table.procs[local].load(Ordering::Acquire);
        let tramp = table.tramps[local].load(Ordering::Acquire);
        Some((f, tramp))
    }

    /// Compile procedure `p`'s definition, once however many threads ask.
    fn tier2_compile(&self, t2: &Tier2, p: u32) {
        let seg_id = p >> SEG;
        let seg = self.seg(p);
        let local = (p & LOCAL) as usize;
        let mut inner = t2.inner.lock().unwrap_or_else(|e| e.into_inner());
        if seg.jit.tier2.state[local].load(Ordering::Acquire) != 0 {
            return;
        }
        // `MEADOW_JIT2_LIMIT=N`: only the first N definitions, for finding
        // the one whose compiled code is at fault.
        static LIMIT: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
        static MADE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        if let Some(n) = *LIMIT.get_or_init(|| {
            std::env::var("MEADOW_JIT2_LIMIT")
                .ok()
                .and_then(|n| n.trim().parse().ok())
        }) && MADE.fetch_add(1, Ordering::Relaxed) >= n
        {
            seg.jit.tier2.state[local].store(2, Ordering::Release);
            return;
        }
        // `MEADOW_JIT2_SKIP=a,b`: not the definitions so named.
        static SKIP: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        let skip = SKIP.get_or_init(|| {
            std::env::var("MEADOW_JIT2_SKIP")
                .map(|s| s.split(',').map(|n| n.trim().to_string()).collect())
                .unwrap_or_default()
        });
        if skip.iter().any(|n| *n == seg.procs[local].name) {
            seg.jit.tier2.state[local].store(2, Ordering::Release);
            return;
        }
        let made = inner.compile(self, seg_id, local);
        let log = std::env::var_os("MEADOW_JIT_LOG").is_some();
        match made {
            Ok((f, methods, arity)) => {
                for (m, at) in methods {
                    let ms = &self.seg(m).jit.tier2.methods;
                    if let Some(slot) = ms.get((m & LOCAL) as usize) {
                        slot.store(at as *mut u8, Ordering::Release);
                    }
                }
                let tramp = match inner.trampoline(arity) {
                    Ok(t) => t,
                    Err(e) => {
                        if log {
                            eprintln!("jit2: {}: {e}", seg.procs[local].name);
                        }
                        seg.jit.tier2.state[local].store(2, Ordering::Release);
                        return;
                    }
                };
                seg.jit.tier2.tramps[local].store(tramp as *mut u8, Ordering::Release);
                let (label, _) = seg.lowered.blocks[local];
                if let Some(slot) = seg.lowered.slots.get(&label)
                    && self.target(*slot) == p
                {
                    t2.slots[*slot as usize].store(f as *mut u8, Ordering::Release);
                }
                seg.jit.tier2.procs[local].store(f as *mut u8, Ordering::Release);
                seg.jit.tier2.state[local].store(1, Ordering::Release);
                if log {
                    eprintln!("jit2: {}", seg.procs[local].name);
                }
            }
            Err(e) => {
                if log {
                    eprintln!("jit2: {}: not compiled: {e}", seg.procs[local].name);
                }
                if std::env::var("MEADOW_JIT_LOG").as_deref() == Ok("linear") {
                    eprintln!("{:#?}", seg.lowered.blocks[local].1);
                }
                // What a failed lowering had queued is not wanted again.
                inner.lowerings.remove(&seg_id);
                seg.jit.tier2.state[local].store(2, Ordering::Release);
            }
        }
    }

    /// Run procedure `p` -- whose slots `env` holds -- as compiled function
    /// `f`, through `tramp`: its answer, or the call it left pending.
    pub(super) fn run_tier2(&self, f: *const u8, tramp: *const u8, p: u32, env: &Env) -> Ran {
        let Some(t2) = self.jit.as_ref().and_then(|j| j.tier2.as_ref()) else {
            return Ran::Not;
        };
        let (threaded, regs) = (t2.threaded, t2.regs);
        let n = self.proc(p).params.len();
        let mut words: SmallVec<[u64; 16]> = SmallVec::new();
        if threaded {
            words.push(meadow_silo::ctx::get() as u64);
        }
        for s in &env[..n] {
            match s {
                V::Val(w) => words.push(*w),
                _ => return Ran::Not,
            }
        }
        // What does not fit in registers goes where the callee loads it.
        if words.len() > regs {
            let area = meadow_silo::meadow_spill_area();
            for (i, w) in words[regs..].iter().enumerate() {
                // Safety: the running thread's spill area, of as many words
                // as the widest call.
                unsafe { *area.add(i) = *w };
            }
            words.truncate(regs);
        }
        // Safety: a trampoline of this many words, calling a function
        // compiled for this procedure.
        let r = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*const u8, *const u64) -> u64>(tramp)(
                f,
                words.as_ptr(),
            )
        };
        if r == SENTINEL
            && let Some(pend) = PENDING.with(|q| q.borrow_mut().take())
        {
            return Ran::Pending(pend);
        }
        Ran::Done(r)
    }

    /// What is pending made the procedure the VM runs next, with `env` its
    /// slots: its number.
    pub(super) fn enter_pending(&self, pend: Pending, env: &mut Env) -> u32 {
        let args: Args = pend.args.iter().map(|w| V::Val(*w)).collect();
        match pend.method {
            None => {
                self.bind(pend.proc, args, env);
                self.safe_point(pend.proc, env);
            }
            Some(obj) => {
                let (m, ncap) = self.method(pend.proc as usize);
                *env = self.enter_method(obj, m, ncap, args);
                self.safe_point(m, env);
                return m;
            }
        }
        pend.proc
    }
}

/// How running compiled code ended.
pub(super) enum Ran {
    Done(Word),
    Pending(Pending),
    /// It was not run: a slot holds what is no word.
    Not,
}

/// A tail call compiled code left for the VM: a definition's procedure, or
/// (with `method`, the closure) a method's number; and the arguments.
pub(super) struct Pending {
    proc: u32,
    method: Option<Word>,
    args: SmallVec<[u64; 12]>,
}

thread_local! {
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
}

/// `n` words at `args`, and those of the `full` the callee takes that the
/// caller put in the spill area.
fn gathered(args: *const u64, n: u64, full: usize) -> SmallVec<[u64; 12]> {
    // Safety: the compiled caller's buffer of `n` words.
    let mut v: SmallVec<[u64; 12]> = unsafe { std::slice::from_raw_parts(args, n as usize) }.into();
    if full > v.len() {
        let area = meadow_silo::meadow_spill_area();
        for i in 0..full - v.len() {
            // Safety: what the caller stored there for this call.
            v.push(unsafe { *area.add(i) });
        }
    }
    v
}

/// The function of method `idx`, compiled, or null for the VM's.
extern "C" fn meadow_jit_method(idx: u64) -> u64 {
    let vm = super::vm();
    let id = idx as u32;
    let Some(seg) = vm.segs.get((id >> SEG) as usize).and_then(|s| s.get()) else {
        return 0;
    };
    seg.jit
        .tier2
        .methods
        .get((id & LOCAL) as usize)
        .map_or(0, |f| f.load(Ordering::Acquire) as u64)
}

/// The definition in `slot`'s procedure, and how many arguments it takes.
fn def_of(slot: u64) -> (u32, usize) {
    let vm = super::vm();
    let p = vm.target(slot as u32);
    (p, vm.proc(p).params.len())
}

extern "C" fn jit_pend_def(slot: u64, args: *const u64, n: u64) -> u64 {
    let (p, full) = def_of(slot);
    let args = gathered(args, n, full);
    PENDING.with(|q| {
        *q.borrow_mut() = Some(Pending {
            proc: p,
            method: None,
            args,
        })
    });
    SENTINEL
}

extern "C" fn jit_call_def(slot: u64, args: *const u64, n: u64) -> u64 {
    let (p, full) = def_of(slot);
    let args = gathered(args, n, full);
    super::vm().call::<true>(p, args.iter().map(|w| V::Val(*w)).collect())
}

/// Method `idx`'s argument count after the closure, the closure among them.
fn method_arity(idx: u64) -> usize {
    let vm = super::vm();
    let (m, ncap) = vm.method(idx as usize);
    1 + vm.proc(m).params.len() - ncap
}

extern "C" fn jit_pend_method(idx: u64, args: *const u64, n: u64) -> u64 {
    let args = gathered(args, n, method_arity(idx));
    PENDING.with(|q| {
        *q.borrow_mut() = Some(Pending {
            proc: idx as u32,
            method: Some(args[0]),
            args: args[1..].into(),
        })
    });
    SENTINEL
}

extern "C" fn jit_call_method(idx: u64, args: *const u64, n: u64) -> u64 {
    let vm = super::vm();
    let args = gathered(args, n, method_arity(idx));
    let (m, ncap) = vm.method(idx as usize);
    let mut env = vm.enter_method(
        args[0],
        m,
        ncap,
        args[1..].iter().map(|w| V::Val(*w)).collect(),
    );
    vm.safe_point(m, &mut env);
    vm.exec::<true>(m, env)
}

/// What a compiled call answered, `r`: the answer, or -- if it left a tail
/// call pending -- what the VM makes of that.
extern "C" fn jit_resume(r: u64) -> u64 {
    let Some(pend) = PENDING.with(|q| q.borrow_mut().take()) else {
        return r;
    };
    let vm = super::vm();
    let mut env = Env::new();
    let p = vm.enter_pending(pend, &mut env);
    vm.exec::<true>(p, env)
}

/// A name of the LIR's, as the module knows it: a segment's own, since two
/// segments' lowerings name their functions alike.
fn mangled(seg: u32, name: &str) -> String {
    format!("s{seg}:{}", name.trim_start_matches('@'))
}

/// The machine's type for one of the LIR's.
fn ty(t: Ty) -> Type {
    match t {
        Ty::I1 | Ty::I8 => types::I8,
        Ty::I16 => types::I16,
        Ty::I32 => types::I32,
        Ty::I64 | Ty::Ptr => types::I64,
        Ty::F32 => types::F32,
        Ty::F64 => types::F64,
        Ty::Void => types::INVALID,
    }
}

fn bytes_of(t: Ty) -> i64 {
    match t {
        Ty::I1 | Ty::I8 => 1,
        Ty::I16 => 2,
        Ty::I32 | Ty::F32 => 4,
        _ => 8,
    }
}

impl Inner {
    /// Definition `def` of segment `seg_id`, compiled: its function, its
    /// methods' by number, and how many words it takes in registers.
    fn compile(
        &mut self,
        vm: &Vm,
        seg_id: u32,
        def: usize,
    ) -> Result<(*const u8, Vec<(u32, *const u8)>, usize), String> {
        let seg = vm.seg(seg_id << SEG);
        if !self.lowerings.contains_key(&seg_id) {
            let l = &seg.lowered;
            let jit = meadow_llvm::emit::Jit {
                bases: l.bases.clone(),
                syms: l.syms.clone(),
                retag: l.retag.clone(),
                global_base: l.global_base,
            };
            let config = vm.state.lock().unwrap_or_else(|e| e.into_inner()).config;
            let m = meadow_llvm::emit::Module::for_jit(
                seg.program,
                meadow_llvm::CallConv::host(),
                config,
                jit,
            );
            if m.threaded() != self.threaded {
                return Err("a segment that starts threads where the first did not".into());
            }
            self.lowerings.insert(seg_id, Box::new(m));
        }
        let lowering = self.lowerings.get_mut(&seg_id).expect("made");
        let (label, block) = seg.lowered.blocks[def];
        let Some((funcs, methods)) = lowering.jit_def(label, block).map_err(|e| e.msg)? else {
            return Err("it makes a closure the VM numbered as it packed it".into());
        };
        let def_name = lowering.label_function(label);
        let strings = lowering.jit_strings();
        let helpers = if self.helpers.contains_key(&seg_id) {
            Vec::new()
        } else {
            lowering.jit_helpers()
        };
        for (g, bytes) in strings {
            let name = mangled(seg_id, &g);
            if self.data.contains_key(&name) {
                continue;
            }
            let id = self
                .module
                .declare_data(&name, Linkage::Local, false, false)
                .map_err(|e| e.to_string())?;
            let mut d = DataDescription::new();
            d.define(bytes.into_boxed_slice());
            self.module.define_data(id, &d).map_err(|e| e.to_string())?;
            self.data.insert(name, id);
        }
        let all: Vec<&Func> = helpers.iter().chain(&funcs).collect();
        for f in &all {
            let name = mangled(seg_id, &f.name);
            let sig = self.signature_of(f);
            let id = self
                .module
                .declare_function(&name, Linkage::Local, &sig)
                .map_err(|e| e.to_string())?;
            self.funcs.insert(name, (id, sig));
        }
        for f in &all {
            self.define(vm, seg_id, f)
                .map_err(|e| format!("{}: {e}", f.name))?;
        }
        self.module
            .finalize_definitions()
            .map_err(|e| e.to_string())?;
        self.helpers.insert(seg_id, true);
        let at = |me: &Inner, name: &str| -> Result<*const u8, String> {
            let (id, _) = me
                .funcs
                .get(&mangled(seg_id, name))
                .ok_or_else(|| format!("{name} was not compiled"))?;
            Ok(me.module.get_finalized_function(*id))
        };
        let f = at(self, &def_name)?;
        let mut ms = Vec::new();
        for (name, id) in methods {
            ms.push((id, at(self, &name)?));
        }
        let params = vm.proc((seg_id << SEG) | def as u32).params.len();
        let arity = params.min(self.regs - usize::from(self.threaded)) + usize::from(self.threaded);
        Ok((f, ms, arity))
    }

    /// How a function of the LIR is called.
    fn signature_of(&self, f: &Func) -> Signature {
        let mut sig = self.module.make_signature();
        if f.cc.is_some() {
            sig.call_conv = CallConv::Tail;
        }
        for (t, _) in &f.params {
            sig.params.push(AbiParam::new(ty(*t)));
        }
        if let Some(r) = f.ret
            && r != Ty::Void
        {
            sig.returns.push(AbiParam::new(ty(r)));
        }
        sig
    }

    /// The way in from Rust to a compiled definition of `words` words in
    /// registers: `extern "C" fn(f, args) -> u64`, which loads them from
    /// `args` and makes the call in the tail convention.
    fn trampoline(&mut self, words: usize) -> Result<*const u8, String> {
        if let Some(t) = self.trampolines.get(&words) {
            return Ok(*t);
        }
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let name = format!("tramp{words}");
        let id = self
            .module
            .declare_function(&name, Linkage::Local, &sig)
            .map_err(|e| e.to_string())?;
        let mut callee = self.module.make_signature();
        callee.call_conv = CallConv::Tail;
        for _ in 0..words {
            callee.params.push(AbiParam::new(types::I64));
        }
        callee.returns.push(AbiParam::new(types::I64));
        self.cl.func.signature = sig;
        let cfg = self.module.target_config();
        {
            let mut b = FunctionBuilder::new(&mut self.cl.func, &mut self.fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let (fp, args) = (b.block_params(entry)[0], b.block_params(entry)[1]);
            let mut vals = Vec::with_capacity(words);
            for i in 0..words {
                vals.push(b.ins().load(
                    types::I64,
                    ir::MemFlagsData::trusted(),
                    args,
                    8 * i as i32,
                ));
            }
            let sref = b.import_signature(callee);
            let call = b.ins().call_indirect(sref, fp, &vals);
            let r = b.inst_results(call)[0];
            b.ins().return_(&[r]);
            b.seal_all_blocks();
            b.finalize(cfg);
        }
        self.module
            .define_function(id, &mut self.cl)
            .map_err(|e| e.to_string())?;
        self.module.clear_context(&mut self.cl);
        self.module
            .finalize_definitions()
            .map_err(|e| e.to_string())?;
        let at = self.module.get_finalized_function(id);
        self.trampolines.insert(words, at);
        Ok(at)
    }

    /// Function `f` of segment `seg`, translated and defined.
    fn define(&mut self, vm: &Vm, seg: u32, f: &Func) -> Result<(), String> {
        let name = mangled(seg, &f.name);
        let (id, sig) = self.funcs.get(&name).cloned().expect("declared");
        self.cl.func.signature = sig;
        let lowered = &vm.seg(seg << SEG).lowered;
        let mut t = Translate {
            module: &mut self.module,
            funcs: &self.funcs,
            data: &self.data,
            seg,
            threaded: self.threaded,
            slots_at: vm
                .jit
                .as_ref()
                .and_then(|j| j.tier2.as_ref())
                .map_or(0, |t| t.slots.as_ptr() as i64),
            labels: &lowered.slots,
            vals: HashMap::new(),
            blocks: HashMap::new(),
            phis: HashMap::new(),
            methods_of: HashMap::new(),
            cur: String::new(),
        };
        let b = FunctionBuilder::new(&mut self.cl.func, &mut self.fctx);
        let made = t.function(b, f);
        if let Err(e) = made {
            self.module.clear_context(&mut self.cl);
            return Err(e);
        }
        if std::env::var("MEADOW_JIT_LOG").as_deref() == Ok("clif") {
            eprintln!("{}", self.cl.func.display());
        }
        self.module
            .define_function(id, &mut self.cl)
            .map_err(|e| format!("{e:?}"))?;
        self.module.clear_context(&mut self.cl);
        Ok(())
    }
}

/// The translation of one function of the LIR into Cranelift's IR.
struct Translate<'a> {
    module: &'a mut JITModule,
    funcs: &'a HashMap<String, (FuncId, Signature)>,
    data: &'a HashMap<String, DataId>,
    seg: u32,
    threaded: bool,
    /// Where the table of compiled definitions by slot is.
    slots_at: i64,
    labels: &'a HashMap<meadow_axcut::Label, u32>,
    /// Each register's value.
    vals: HashMap<String, Value>,
    /// Each block of the LIR's, by its label.
    blocks: HashMap<String, Block>,
    /// Each block's phis: its parameters, as each predecessor gives them.
    phis: HashMap<String, Vec<(Ty, Vec<(String, String)>)>>,
    /// A function pointer a method's number gave, and that number: a call
    /// through it is the VM's when the method is not compiled.
    methods_of: HashMap<String, Value>,
    /// The LIR block being translated.
    cur: String,
}

impl Translate<'_> {
    fn function(&mut self, mut b: FunctionBuilder, f: &Func) -> Result<(), String> {
        // The blocks, and each one's phis.
        let insts: Vec<&Inst> = f.insts().collect();
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        self.blocks.insert("entry".into(), entry);
        let mut label = "entry".to_string();
        for i in &insts {
            match i {
                Inst::Label(l) => {
                    let blk = b.create_block();
                    self.blocks.insert(l.clone(), blk);
                    label = l.clone();
                }
                Inst::Phi {
                    ty: t,
                    incoming,
                    dst,
                } => {
                    let blk = self.blocks[&label];
                    let v = b.append_block_param(blk, ty(*t));
                    self.vals.insert(dst.clone(), v);
                    self.phis
                        .entry(label.clone())
                        .or_default()
                        .push((*t, incoming.clone()));
                }
                _ => {}
            }
        }
        b.switch_to_block(entry);
        for ((_, p), v) in f.params.iter().zip(b.block_params(entry).to_vec()) {
            self.vals.insert(p.clone(), v);
        }
        self.cur = "entry".into();
        let mut ended = false;
        let mut k = 0;
        while k < insts.len() {
            let i = insts[k];
            match i {
                Inst::Label(l) => {
                    if !ended {
                        // Falling into a block: what LLVM does not allow,
                        // and the lowering never writes.
                        return Err(format!("a block that falls into `{l}`"));
                    }
                    b.switch_to_block(self.blocks[l]);
                    self.cur = l.clone();
                    ended = false;
                }
                Inst::Phi { .. } => {}
                Inst::Call {
                    dst: Some(d),
                    tail: true,
                    ..
                } if matches!(insts.get(k + 1), Some(Inst::Ret { v: Some(r), .. }) if r == d) => {
                    // A tail call and the return of what it answers: one.
                    self.call(&mut b, i, true)?;
                    ended = true;
                    k += 2;
                    continue;
                }
                _ => {
                    ended = self.inst(&mut b, i)?;
                }
            }
            k += 1;
        }
        b.seal_all_blocks();
        b.finalize(self.module.target_config());
        Ok(())
    }

    /// The value operand `v` is, of type `t`.
    fn val(&mut self, b: &mut FunctionBuilder, v: &str, t: Ty) -> Result<Value, String> {
        let want = ty(t);
        Ok(
            match Operand::parse(v).ok_or_else(|| format!("an operand `{v}`"))? {
                Operand::Reg(_) => *self
                    .vals
                    .get(v)
                    .ok_or_else(|| format!("`{v}` used before it is defined"))?,
                Operand::Int(n) => {
                    if want == types::F64 {
                        b.ins().f64const(n as f64)
                    } else {
                        b.ins().iconst(want, n)
                    }
                }
                Operand::Float(bits) => b.ins().f64const(f64::from_bits(bits)),
                Operand::Null | Operand::Undef => {
                    if want == types::F64 {
                        b.ins().f64const(0.0)
                    } else {
                        b.ins().iconst(want, 0)
                    }
                }
                Operand::Global(g) => self.address(b, g)?,
            },
        )
    }

    /// The address of global `g`: a function's, a string's, or the
    /// runtime's.
    fn address(&mut self, b: &mut FunctionBuilder, g: &str) -> Result<Value, String> {
        let name = mangled(self.seg, g);
        if let Some((id, _)) = self.funcs.get(&name) {
            let fr = self.module.declare_func_in_func(*id, b.func);
            return Ok(b.ins().func_addr(types::I64, fr));
        }
        if let Some(id) = self.data.get(&name) {
            let gv = self.module.declare_data_in_func(*id, b.func);
            return Ok(b.ins().symbol_value(types::I64, gv));
        }
        // The runtime's statics.
        let id = self
            .module
            .declare_data(g, Linkage::Import, true, false)
            .map_err(|e| e.to_string())?;
        let gv = self.module.declare_data_in_func(id, b.func);
        Ok(b.ins().symbol_value(types::I64, gv))
    }

    fn set(&mut self, d: &str, v: Value) {
        self.vals.insert(d.to_string(), v);
    }

    /// The arguments block `to`'s phis take from the block being left.
    fn edge(&mut self, b: &mut FunctionBuilder, to: &str) -> Result<Vec<ir::BlockArg>, String> {
        let Some(ps) = self.phis.get(to).cloned() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(ps.len());
        for (t, incoming) in ps {
            let (v, _) = incoming
                .iter()
                .find(|(_, from)| *from == self.cur)
                .ok_or_else(|| format!("`{to}` has no phi value from `{}`", self.cur))?;
            out.push(ir::BlockArg::Value(self.val(b, v, t)?));
        }
        Ok(out)
    }

    fn block(&self, l: &str) -> Result<Block, String> {
        self.blocks
            .get(l)
            .copied()
            .ok_or_else(|| format!("no block `{l}`"))
    }

    /// One instruction: whether it ended its block.
    fn inst(&mut self, b: &mut FunctionBuilder, i: &Inst) -> Result<bool, String> {
        match i {
            Inst::Bin {
                dst,
                op,
                ty: t,
                a,
                b: y,
            } => {
                let (x, y) = (self.val(b, a, *t)?, self.val(b, y, *t)?);
                let r = match op {
                    BinOp::Add => b.ins().iadd(x, y),
                    BinOp::Sub => b.ins().isub(x, y),
                    BinOp::Mul => b.ins().imul(x, y),
                    BinOp::SDiv => b.ins().sdiv(x, y),
                    BinOp::SRem => b.ins().srem(x, y),
                    BinOp::UDiv => b.ins().udiv(x, y),
                    BinOp::URem => b.ins().urem(x, y),
                    BinOp::And => b.ins().band(x, y),
                    BinOp::Or => b.ins().bor(x, y),
                    BinOp::Xor => b.ins().bxor(x, y),
                    BinOp::Shl => b.ins().ishl(x, y),
                    BinOp::LShr => b.ins().ushr(x, y),
                    BinOp::AShr => b.ins().sshr(x, y),
                    BinOp::FAdd => b.ins().fadd(x, y),
                    BinOp::FSub => b.ins().fsub(x, y),
                    BinOp::FMul => b.ins().fmul(x, y),
                    BinOp::FDiv => b.ins().fdiv(x, y),
                    BinOp::FRem => return Err("`frem`, which the machine has not".into()),
                };
                self.set(dst, r);
            }
            Inst::ICmp {
                dst,
                cond,
                ty: t,
                a,
                b: y,
            } => {
                let (x, y) = (self.val(b, a, *t)?, self.val(b, y, *t)?);
                let cc = match cond {
                    ICmp::Eq => IntCC::Equal,
                    ICmp::Ne => IntCC::NotEqual,
                    ICmp::Slt => IntCC::SignedLessThan,
                    ICmp::Sle => IntCC::SignedLessThanOrEqual,
                    ICmp::Sgt => IntCC::SignedGreaterThan,
                    ICmp::Sge => IntCC::SignedGreaterThanOrEqual,
                    ICmp::Ult => IntCC::UnsignedLessThan,
                    ICmp::Ule => IntCC::UnsignedLessThanOrEqual,
                    ICmp::Ugt => IntCC::UnsignedGreaterThan,
                    ICmp::Uge => IntCC::UnsignedGreaterThanOrEqual,
                };
                let r = b.ins().icmp(cc, x, y);
                self.set(dst, r);
            }
            Inst::FCmp {
                dst,
                cond,
                ty: t,
                a,
                b: y,
            } => {
                let (x, y) = (self.val(b, a, *t)?, self.val(b, y, *t)?);
                let cc = match cond {
                    FCmp::Oeq => FloatCC::Equal,
                    FCmp::One => FloatCC::OrderedNotEqual,
                    FCmp::Olt => FloatCC::LessThan,
                    FCmp::Ole => FloatCC::LessThanOrEqual,
                    FCmp::Ogt => FloatCC::GreaterThan,
                    FCmp::Oge => FloatCC::GreaterThanOrEqual,
                    FCmp::Une => FloatCC::NotEqual,
                };
                let r = b.ins().fcmp(cc, x, y);
                self.set(dst, r);
            }
            Inst::Cast {
                dst,
                op,
                from,
                v,
                to,
            } => {
                let x = self.val(b, v, *from)?;
                let (f, t) = (ty(*from), ty(*to));
                let r = match op {
                    Cast::IntToPtr | Cast::PtrToInt => x,
                    Cast::Bitcast if f == t => x,
                    Cast::Bitcast => b.ins().bitcast(t, ir::MemFlagsData::new(), x),
                    Cast::ZExt if f == t => x,
                    Cast::ZExt => b.ins().uextend(t, x),
                    Cast::SExt if f == t => x,
                    Cast::SExt => b.ins().sextend(t, x),
                    Cast::Trunc if f == t => x,
                    Cast::Trunc => b.ins().ireduce(t, x),
                    Cast::SIToFP => b.ins().fcvt_from_sint(t, x),
                    Cast::UIToFP => b.ins().fcvt_from_uint(t, x),
                    Cast::FPToSI => b.ins().fcvt_to_sint_sat(t, x),
                    Cast::FPToUI => b.ins().fcvt_to_uint_sat(t, x),
                    Cast::FPExt => b.ins().fpromote(t, x),
                    Cast::FPTrunc => b.ins().fdemote(t, x),
                };
                self.set(dst, r);
            }
            Inst::Load {
                dst, ty: t, ptr, ..
            } => {
                let p = self.val(b, ptr, Ty::Ptr)?;
                let r = b.ins().load(ty(*t), ir::MemFlagsData::trusted(), p, 0);
                self.set(dst, r);
            }
            Inst::Store { ty: t, v, ptr, .. } => {
                let (x, p) = (self.val(b, v, *t)?, self.val(b, ptr, Ty::Ptr)?);
                b.ins().store(ir::MemFlagsData::trusted(), x, p, 0);
            }
            Inst::Gep {
                dst,
                elem,
                base,
                index,
            } => {
                let p = self.val(b, base, Ty::Ptr)?;
                let r = match Operand::parse(index) {
                    Some(Operand::Int(n)) => b.ins().iadd_imm_s(p, n * bytes_of(*elem)),
                    _ => {
                        let x = self.val(b, index, Ty::I64)?;
                        let off = b.ins().imul_imm_s(x, bytes_of(*elem));
                        b.ins().iadd(p, off)
                    }
                };
                self.set(dst, r);
            }
            Inst::Alloca { dst, words } => {
                let slot = b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (8 * (*words).max(1)) as u32,
                    3,
                ));
                let r = b.ins().stack_addr(types::I64, slot, 0);
                self.set(dst, r);
            }
            Inst::Select {
                dst,
                cond,
                ty: t,
                a,
                b: y,
            } => {
                let c = self.val(b, cond, Ty::I1)?;
                let (x, y) = (self.val(b, a, *t)?, self.val(b, y, *t)?);
                let r = b.ins().select(c, x, y);
                self.set(dst, r);
            }
            Inst::Phi { .. } | Inst::Label(_) => {}
            Inst::Call { .. } => self.call(b, i, false)?,
            Inst::Br(to) => {
                let args = self.edge(b, to)?;
                let blk = self.block(to)?;
                b.ins().jump(blk, &args);
                return Ok(true);
            }
            Inst::CondBr { cond, yes, no, .. } => {
                let c = self.val(b, cond, Ty::I1)?;
                let (ya, na) = (self.edge(b, yes)?, self.edge(b, no)?);
                let (yb, nb) = (self.block(yes)?, self.block(no)?);
                b.ins().brif(c, yb, &ya, nb, &na);
                return Ok(true);
            }
            Inst::Switch {
                ty: t,
                v,
                default,
                cases,
            } => {
                let x = self.val(b, v, *t)?;
                // A target with phis is reached through a block of its own
                // that gives them their values.
                let through =
                    |me: &mut Self, b: &mut FunctionBuilder, to: &str| -> Result<Block, String> {
                        let args = me.edge(b, to)?;
                        let blk = me.block(to)?;
                        if args.is_empty() {
                            return Ok(blk);
                        }
                        let here = b.current_block().expect("in a block");
                        let edge = b.create_block();
                        b.switch_to_block(edge);
                        b.ins().jump(blk, &args);
                        b.switch_to_block(here);
                        Ok(edge)
                    };
                let mut sw = Switch::new();
                for (k, to) in cases {
                    let blk = through(self, b, to)?;
                    sw.set_entry(*k as u64 as u128, blk);
                }
                let otherwise = through(self, b, default)?;
                sw.emit(b, x, otherwise);
                return Ok(true);
            }
            Inst::Ret { ty: t, v } => {
                match v {
                    Some(v) => {
                        let x = self.val(b, v, *t)?;
                        b.ins().return_(&[x]);
                    }
                    None => {
                        b.ins().return_(&[]);
                    }
                }
                return Ok(true);
            }
            Inst::Unreachable => {
                b.ins().trap(ir::TrapCode::unwrap_user(1));
                return Ok(true);
            }
            Inst::Raw(text) => return Err(format!("LLVM text the LIR has not said: {text}")),
        }
        Ok(false)
    }

    /// The signature of a call of `ret` with `args`, in the tail convention
    /// if `tail_cc`.
    fn sig(&self, tail_cc: bool, ret: Ty, args: &[(Ty, String)]) -> Signature {
        let mut s = self.module.make_signature();
        if tail_cc {
            s.call_conv = CallConv::Tail;
        }
        for (t, _) in args {
            s.params.push(AbiParam::new(ty(*t)));
        }
        if ret != Ty::Void {
            s.returns.push(AbiParam::new(ty(ret)));
        }
        s
    }

    /// A call: `tail` when it is a tail call followed by the return of what
    /// it answers, which this makes one.
    fn call(&mut self, b: &mut FunctionBuilder, i: &Inst, tail: bool) -> Result<(), String> {
        let Inst::Call {
            dst,
            cc,
            ret,
            callee,
            args,
            ..
        } = i
        else {
            unreachable!("a call")
        };
        let tail_cc = cc.is_some();
        let mut vals = Vec::with_capacity(args.len());
        for (t, a) in args {
            vals.push(self.val(b, a, *t)?);
        }
        let answer = |me: &mut Self, b: &mut FunctionBuilder, r: Option<Value>| {
            if let (Some(d), Some(r)) = (dst, r) {
                me.set(d, r);
            }
            let _ = b;
        };
        if callee == "@llvm.stacksave" {
            let sp = b.ins().get_stack_pointer(types::I64);
            answer(self, b, Some(sp));
            return Ok(());
        }
        // A method by its number: compiled, or the VM's.
        if let Some(idx) = self.methods_of.get(callee).copied() {
            return self.bridged(b, i, tail, Bridge::Method(idx), vals);
        }
        if callee.starts_with('%') {
            let fp = self.val(b, callee, Ty::Ptr)?;
            let sref = b.import_signature(self.sig(tail_cc, *ret, args));
            if tail {
                b.ins().return_call_indirect(sref, fp, &vals);
                return Ok(());
            }
            let c = b.ins().call_indirect(sref, fp, &vals);
            let r = b.inst_results(c).first().copied();
            let r = self.resumed(b, tail_cc, r);
            answer(self, b, r);
            return Ok(());
        }
        let name = callee.trim_start_matches('@');
        // A definition by name: through its slot.
        if let Some(n) = name.strip_prefix("mw.L")
            && let Ok(n) = n.parse::<u32>()
            && !self.funcs.contains_key(&mangled(self.seg, callee))
        {
            let slot = *self
                .labels
                .get(&meadow_axcut::Label(n))
                .ok_or_else(|| format!("no slot for {callee}"))?;
            return self.bridged(b, i, tail, Bridge::Def(slot), vals);
        }
        let fref = match self.funcs.get(&mangled(self.seg, callee)) {
            Some((id, _)) => self.module.declare_func_in_func(*id, b.func),
            None => {
                let sig = self.sig(tail_cc, *ret, args);
                let id = self
                    .module
                    .declare_function(name, Linkage::Import, &sig)
                    .map_err(|e| format!("{name}: {e}"))?;
                self.module.declare_func_in_func(id, b.func)
            }
        };
        if tail && tail_cc {
            b.ins().return_call(fref, &vals);
            return Ok(());
        }
        let c = b.ins().call(fref, &vals);
        let r = b.inst_results(c).first().copied();
        if name == "meadow_jit_method"
            && let (Some(d), Some(_)) = (dst, r)
        {
            self.methods_of.insert(d.clone(), vals[0]);
        }
        let r = self.resumed(b, tail_cc, r);
        if tail {
            match r {
                Some(r) => b.ins().return_(&[r]),
                None => b.ins().return_(&[]),
            };
            return Ok(());
        }
        answer(self, b, r);
        Ok(())
    }

    /// What a call in the tail convention answered, `r`, once what it may
    /// have left pending is run: see the module docs.
    fn resumed(
        &mut self,
        b: &mut FunctionBuilder,
        tail_cc: bool,
        r: Option<Value>,
    ) -> Option<Value> {
        let r = r?;
        if !tail_cc || b.func.dfg.value_type(r) != types::I64 {
            return Some(r);
        }
        let (slow, on) = (b.create_block(), b.create_block());
        let v = b.append_block_param(on, types::I64);
        let pending = b.ins().icmp_imm_s(IntCC::Equal, r, SENTINEL as i64);
        b.ins()
            .brif(pending, slow, &[], on, &[ir::BlockArg::Value(r)]);
        b.switch_to_block(slow);
        let r2 = self.runtime_call(b, "meadow_jit_resume", &[types::I64], &[r]);
        b.ins().jump(on, &[ir::BlockArg::Value(r2)]);
        b.switch_to_block(on);
        Some(v)
    }

    /// A call of one of the runtime's functions of `params`, answering a
    /// word.
    fn runtime_call(
        &mut self,
        b: &mut FunctionBuilder,
        name: &str,
        params: &[Type],
        args: &[Value],
    ) -> Value {
        let mut sig = self.module.make_signature();
        for p in params {
            sig.params.push(AbiParam::new(*p));
        }
        sig.returns.push(AbiParam::new(types::I64));
        let id = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .expect("the runtime's");
        let fref = self.module.declare_func_in_func(id, b.func);
        let c = b.ins().call(fref, args);
        b.inst_results(c)[0]
    }

    /// A call of a definition by its slot, or of a method by its number:
    /// compiled code where there is some, the VM where there is not.
    fn bridged(
        &mut self,
        b: &mut FunctionBuilder,
        i: &Inst,
        tail: bool,
        to: Bridge,
        vals: Vec<Value>,
    ) -> Result<(), String> {
        let Inst::Call { dst, ret, args, .. } = i else {
            unreachable!("a call")
        };
        let sref = b.import_signature(self.sig(true, *ret, args));
        let fp = match to {
            Bridge::Def(slot) => {
                let table = b
                    .ins()
                    .iconst(types::I64, self.slots_at + 8 * i64::from(slot));
                b.ins()
                    .load(types::I64, ir::MemFlagsData::trusted(), table, 0)
            }
            Bridge::Method(_) => {
                let fp = self.vals.get(match i {
                    Inst::Call { callee, .. } => callee.as_str(),
                    _ => unreachable!(),
                });
                *fp.ok_or("a method's function before it is asked for")?
            }
        };
        let (native, vm) = (b.create_block(), b.create_block());
        let on = (!tail).then(|| b.create_block());
        let v = on.map(|on| b.append_block_param(on, types::I64));
        b.ins().brif(fp, native, &[], vm, &[]);
        // Compiled.
        b.switch_to_block(native);
        if tail {
            b.ins().return_call_indirect(sref, fp, &vals);
        } else {
            let c = b.ins().call_indirect(sref, fp, &vals);
            let r = b.inst_results(c)[0];
            let r = self.resumed(b, true, Some(r)).expect("an answer");
            b.ins()
                .jump(on.expect("a call that waits"), &[ir::BlockArg::Value(r)]);
        }
        // The VM's: the arguments in a buffer, without the context.
        b.switch_to_block(vm);
        let words: Vec<Value> = vals[usize::from(self.threaded)..].to_vec();
        let buf = b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            (8 * words.len().max(1)) as u32,
            3,
        ));
        for (k, w) in words.iter().enumerate() {
            b.ins().stack_store(types::I64, *w, buf, 8 * k as i32);
        }
        let at = b.ins().stack_addr(types::I64, buf, 0);
        let n = b.ins().iconst(types::I64, words.len() as i64);
        let (helper, key) = match (to, tail) {
            (Bridge::Def(slot), true) => (
                "meadow_jit_pend_def",
                b.ins().iconst(types::I64, i64::from(slot)),
            ),
            (Bridge::Def(slot), false) => (
                "meadow_jit_call_def",
                b.ins().iconst(types::I64, i64::from(slot)),
            ),
            (Bridge::Method(idx), true) => ("meadow_jit_pend_method", idx),
            (Bridge::Method(idx), false) => ("meadow_jit_call_method", idx),
        };
        let r = self.runtime_call(b, helper, &[types::I64; 3], &[key, at, n]);
        let Some(on) = on else {
            b.ins().return_(&[r]);
            return Ok(());
        };
        b.ins().jump(on, &[ir::BlockArg::Value(r)]);
        b.switch_to_block(on);
        if let (Some(d), Some(v)) = (dst, v) {
            self.set(d, v);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Bridge {
    Def(u32),
    Method(Value),
}
