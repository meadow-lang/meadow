//! **Native code for a running program**: ahead of time, or as it runs.
//!
//! [`Native`] is what a machine consults at each block it enters (see
//! [`crate::Vm::advance`]): the function compiled for the block, if there is
//! one. Every thread of a run shares one, so a block compiled on one thread
//! is native on all of them.
//!
//! # Ahead of time
//!
//! [`Native::ahead_of_time`]: every block's function, from an executable the
//! linker made (see [`crate::aot`]). Nothing is compiled while the program runs.
//!
//! # Just in time
//!
//! [`Native::jit`]: nothing, to begin with. The machine interprets, and counts
//! the times it enters each block; a block entered `threshold` times is
//! compiled then (see [`crate::codegen::compile_block`]) -- under a lock, so
//! it is compiled once however many threads get there -- and run natively from
//! then on, by every thread. Code that runs once -- most of what a program's
//! start does -- is never compiled at all, and what runs in a loop is compiled
//! within a few iterations.
//!
//! The translation is the same either way, and needs no relocation: a block's
//! function can go anywhere, which is what lets it be compiled on its own.
//!
//! # Executable memory
//!
//! Platform business, behind one rule: functions are packed together, as a
//! linker lays them out, and no address is ever both writable and executable.
//! On macOS it is `MAP_JIT` memory, writable only by the thread writing it,
//! for as long as it writes -- other threads go on running what is there --
//! and on Apple silicon the processor's cached copy of the instructions is
//! told they changed. Elsewhere each chunk is mapped twice from the same
//! memory, a view to write and a view to run: a `memfd` on Linux, a section
//! backed by the page file on Windows: `meadow_exec`'s [`Arena`], which says
//! why packing matters.

use crate::abi::NativeFn;
use crate::codegen::{self, Arch};
use meadow_bytecode::{Pc, Program};
use meadow_exec::Arena;
use meadow_rt::OptLevel;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Native code for a program's blocks, by entry pc.
pub struct Native<'p> {
    pub(crate) table: Box<[AtomicPtr<c_void>]>,
    /// The method entry of each pc that begins a method, or null: see
    /// [`crate::Vm::use_native`].
    pub(crate) methods: Box<[AtomicPtr<c_void>]>,
    tier: Option<Tier<'p>>,
    /// The program's method tables, flattened for native code: see
    /// [`crate::Vm::use_native`].
    pub(crate) method_pcs: Box<[u32]>,
    pub(crate) method_starts: Box<[u32]>,
}

/// Every method table's pcs in a row, and where each table starts (and the
/// last one ends).
fn methods(program: &Program) -> (Box<[u32]>, Box<[u32]>) {
    let mut pcs = Vec::new();
    let mut starts = Vec::with_capacity(program.methods.len() + 1);
    for table in &program.methods {
        starts.push(pcs.len() as u32);
        pcs.extend_from_slice(table);
    }
    starts.push(pcs.len() as u32);
    (pcs.into_boxed_slice(), starts.into_boxed_slice())
}

/// What compiling as the program runs needs.
struct Tier<'p> {
    program: &'p Program,
    arch: Arch,
    opt: OptLevel,
    /// Which pcs start a block.
    entry: Box<[bool]>,
    /// What every block's compilation needs to know of the whole program:
    /// once per program, not per block. See `codegen::Whole`.
    whole: codegen::Whole,
    counts: Box<[AtomicU32]>,
    threshold: u32,
    /// Per `invoke` site: what it has entered while interpreted -- see
    /// [`Native::observe`].
    calls: Box<[AtomicU64]>,
    /// Whether a block's calls are guarded on what they were seen to enter:
    /// unless `MEADOW_JIT_GUESS=0`, which is for telling whether a guess is
    /// what broke something.
    guess: bool,
    code: Mutex<Arena>,
    compiled: AtomicUsize,
}

/// A call site's word, once it has entered something: see [`Native::observe`].
const SEEN: u64 = 1 << 62;
const FRAME_BIT: u32 = 61;
/// It has entered more than one thing.
const POLY: u64 = u64::MAX;

/// What a site's word says it entered, if one thing.
fn known(w: u64) -> Option<codegen::Known> {
    (w != POLY && w & SEEN != 0).then(|| codegen::Known {
        frame: w >> FRAME_BIT & 1 != 0,
        meta: w as u32,
    })
}

/// How many entries make a block hot, unless `MEADOW_JIT_THRESHOLD` says.
pub const DEFAULT_THRESHOLD: u32 = 16;

impl<'p> Native<'p> {
    /// Functions compiled ahead of time: `(entry pc, function)` for the
    /// blocks of a program of `len` instructions.
    pub fn ahead_of_time(
        program: &Program,
        blocks: impl IntoIterator<Item = (Pc, NativeFn)>,
        stubs: impl IntoIterator<Item = (Pc, *const u8)>,
    ) -> Native<'p> {
        let len = program.code.len();
        let table: Box<[AtomicPtr<c_void>]> = (0..len)
            .map(|_| AtomicPtr::new(std::ptr::null_mut()))
            .collect();
        for (pc, f) in blocks {
            if let Some(slot) = table.get(pc as usize) {
                slot.store(f as *mut c_void, Ordering::Release);
            }
        }
        let entries: Box<[AtomicPtr<c_void>]> = (0..len)
            .map(|_| AtomicPtr::new(std::ptr::null_mut()))
            .collect();
        for (pc, at) in stubs {
            if let Some(slot) = entries.get(pc as usize) {
                slot.store(at as *mut c_void, Ordering::Release);
            }
        }
        let (method_pcs, method_starts) = methods(program);
        Native {
            table,
            methods: entries,
            tier: None,
            method_pcs,
            method_starts,
        }
    }

    /// Compile `program`'s blocks as they get hot, at `opt`: each the
    /// `threshold`th time the machine enters it. A threshold of 1 compiles every
    /// block that runs, when it first does.
    pub fn jit(program: &'p Program, threshold: u32, opt: OptLevel) -> Result<Native<'p>, String> {
        let arch = Arch::host().ok_or("the JIT is for aarch64 and x86-64 only")?;
        let len = program.code.len();
        let mut entry = vec![false; len].into_boxed_slice();
        for pc in crate::abi::block_entries(program) {
            entry[pc as usize] = true;
        }
        let (method_pcs, method_starts) = methods(program);
        Ok(Native {
            method_pcs,
            method_starts,
            table: (0..len)
                .map(|_| AtomicPtr::new(std::ptr::null_mut()))
                .collect(),
            methods: (0..len)
                .map(|_| AtomicPtr::new(std::ptr::null_mut()))
                .collect(),
            tier: Some(Tier {
                program,
                arch,
                opt,
                entry,
                whole: codegen::Whole::of(program),
                counts: (0..len).map(|_| AtomicU32::new(0)).collect(),
                calls: (0..len).map(|_| AtomicU64::new(0)).collect(),
                guess: std::env::var_os("MEADOW_JIT_GUESS").is_none_or(|v| v != "0"),
                threshold: threshold.max(1),
                code: Mutex::new(Arena::default()),
                compiled: AtomicUsize::new(0),
            }),
        })
    }

    /// Every block of `program`, compiled now, at `opt`.
    pub fn eager(program: &'p Program, opt: OptLevel) -> Result<Native<'p>, String> {
        let native = Native::jit(program, 1, opt)?;
        let tier = native.tier.as_ref().expect("a JIT has a tier");
        for pc in crate::abi::block_entries(program) {
            native.compile(tier, pc as usize);
        }
        Ok(native)
    }

    /// The threshold `MEADOW_JIT_THRESHOLD` names, or [`DEFAULT_THRESHOLD`].
    pub fn threshold_from_env() -> u32 {
        std::env::var("MEADOW_JIT_THRESHOLD")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(DEFAULT_THRESHOLD)
    }

    /// The function for the block at `pc`, if it has one -- compiling it, if
    /// this entry makes it hot.
    #[inline]
    pub fn at(&self, pc: usize) -> Option<NativeFn> {
        let p = self.table.get(pc)?.load(Ordering::Acquire);
        if !p.is_null() {
            // Safety: only ever a compiled function's address goes in.
            return Some(unsafe { std::mem::transmute::<*mut c_void, NativeFn>(p) });
        }
        let tier = self.tier.as_ref()?;
        if !tier.entry[pc] {
            return None;
        }
        let n = tier.counts[pc].fetch_add(1, Ordering::Relaxed) + 1;
        if n < tier.threshold {
            return None;
        }
        self.compile(tier, pc)
    }

    /// Note that the `invoke` at `site` entered `known`, while its block was
    /// interpreted. What a block's compilation guesses its calls enter -- see
    /// [`codegen::Known`] -- and what [`Native::calls`] hands on to a build
    /// ahead of time.
    ///
    /// One word per site, and nothing to take a lock for: empty, then the one
    /// thing seen, then [`POLY`] for good once a second one is.
    #[inline]
    pub fn observe(&self, site: usize, known: codegen::Known) {
        let Some(slot) = self.tier.as_ref().and_then(|t| t.calls.get(site)) else {
            return;
        };
        let want = SEEN | (known.frame as u64) << FRAME_BIT | known.meta as u64;
        let now = slot.load(Ordering::Relaxed);
        if now == want || now == POLY {
            return;
        }
        if now == 0
            && slot
                .compare_exchange(0, want, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            return;
        }
        if slot.load(Ordering::Relaxed) != want {
            slot.store(POLY, Ordering::Relaxed);
        }
    }

    /// Every `invoke` site that entered one thing, and only one, while it was
    /// interpreted.
    pub fn calls(&self) -> codegen::Calls {
        let Some(tier) = &self.tier else {
            return codegen::Calls::new();
        };
        tier.calls
            .iter()
            .enumerate()
            .filter_map(|(pc, w)| Some((pc as Pc, known(w.load(Ordering::Relaxed))?)))
            .collect()
    }

    /// Blocks compiled so far, as the program runs.
    pub fn compiled(&self) -> usize {
        self.tier
            .as_ref()
            .map_or(0, |t| t.compiled.load(Ordering::Relaxed))
    }

    #[cold]
    fn compile(&self, tier: &Tier<'p>, pc: usize) -> Option<NativeFn> {
        let mut arena = tier.code.lock().unwrap_or_else(|e| e.into_inner());
        let slot = &self.table[pc];
        let p = slot.load(Ordering::Acquire);
        if !p.is_null() {
            // Another thread got here first.
            return Some(unsafe { std::mem::transmute::<*mut c_void, NativeFn>(p) });
        }
        let (code, stub) = codegen::compile_block(
            tier.program,
            tier.arch,
            pc as Pc,
            tier.opt,
            &tier.whole,
            &|site| {
                if !tier.guess {
                    return None;
                }
                known(tier.calls.get(site as usize)?.load(Ordering::Relaxed))
            },
        );
        let Some(at) = arena.put(&code, align(tier.arch)) else {
            // No memory to put it in: interpret it, and stop asking.
            tier.counts[pc].store(0, Ordering::Relaxed);
            return None;
        };
        if let Some(offset) = stub {
            // Safety: an offset into the function just written.
            let entry = unsafe { at.add(offset as usize) };
            self.methods[pc].store(entry as *mut c_void, Ordering::Release);
        }
        slot.store(at as *mut c_void, Ordering::Release);
        tier.compiled.fetch_add(1, Ordering::Relaxed);
        // Safety: the function just written there.
        Some(unsafe { std::mem::transmute::<*mut u8, NativeFn>(at) })
    }
}

/// Where a function starts: where the architecture's fetch likes it.
fn align(arch: Arch) -> usize {
    match arch {
        Arch::Aarch64 => 4,
        Arch::X86_64 => 16,
    }
}
