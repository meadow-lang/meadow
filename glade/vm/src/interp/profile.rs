//! **A sampling profile** of a program the VM runs: where it spends itself,
//! as folded stacks, one line a stack, for `flamegraph.pl` or speedscope --
//! what `meadow run --profile-to` writes for Glade's VM, the same way.
//!
//! The VM keeps a record of each activation, linked to its caller's, when it
//! is asked to: the debugger's (`debug::Act`). Profiled, it keeps them
//! without a debugger, and between instructions looks to see whether a
//! sample is due -- by time, `hz` times a second of the clock, which it reads
//! every thousand instructions or so, or by work, every `every` instructions,
//! the same profile every time. A sample
//! is the chain of records, innermost first, as far as `depth`: procedures,
//! named at the end by the definitions they are of.

use super::debug::Act;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// How samples are taken: see the module docs.
pub struct How {
    pub every: Option<u64>,
    pub hz: u64,
    pub depth: usize,
}

static ON: AtomicBool = AtomicBool::new(false);
static EVERY: AtomicU64 = AtomicU64::new(0);
static DEPTH: AtomicU64 = AtomicU64::new(64);
/// Instructions since the last sample, by work, or since the clock was last
/// read, by time; and when the next is due, by time, in nanoseconds since
/// the profile began.
static SINCE: AtomicU64 = AtomicU64::new(0);
static NEXT: AtomicU64 = AtomicU64::new(0);
static PERIOD: AtomicU64 = AtomicU64::new(1_000_000);
static BEGAN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
/// How many instructions go between looks at the clock: few enough that a
/// sample is late by microseconds, many enough that looking costs nothing.
const LOOK: u64 = 1024;
/// Each stack sampled, as procedures innermost first, and how often.
static STACKS: Mutex<Option<HashMap<Vec<u32>, u64>>> = Mutex::new(None);

/// Profile the program the VM runs, as `how` says. Before it is loaded: it
/// keeps its activations' records only if it is asked to then.
pub fn start(how: How) {
    ON.store(true, Ordering::Relaxed);
    DEPTH.store(how.depth.max(1) as u64, Ordering::Relaxed);
    *STACKS.lock().unwrap_or_else(|p| p.into_inner()) = Some(HashMap::new());
    match how.every {
        Some(n) => EVERY.store(n.max(1), Ordering::Relaxed),
        // By the clock, which the program reads now and then: a timer's
        // sleep is as coarse as the system's tick, a sixtieth of a second on
        // Windows.
        None => {
            let period = 1_000_000_000 / how.hz.max(1);
            PERIOD.store(period, Ordering::Relaxed);
            NEXT.store(period, Ordering::Relaxed);
            let _ = BEGAN.set(std::time::Instant::now());
        }
    }
}

/// Whether the program is being profiled.
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Before the instruction `act` is at: take a sample if one is due.
#[inline(always)]
pub(super) fn tick(act: &Act) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let every = EVERY.load(Ordering::Relaxed);
    let due = if every == 0 {
        SINCE.fetch_add(1, Ordering::Relaxed) + 1 >= LOOK && {
            SINCE.store(0, Ordering::Relaxed);
            let now = BEGAN.get().map_or(0, |b| b.elapsed().as_nanos() as u64);
            let next = NEXT.load(Ordering::Relaxed);
            now >= next && {
                NEXT.store(now + PERIOD.load(Ordering::Relaxed), Ordering::Relaxed);
                true
            }
        }
    } else {
        SINCE.fetch_add(1, Ordering::Relaxed) + 1 >= every && {
            SINCE.store(0, Ordering::Relaxed);
            true
        }
    };
    if due {
        sample(act);
    }
}

#[cold]
fn sample(act: &Act) {
    let depth = DEPTH.load(Ordering::Relaxed) as usize;
    let mut stack = Vec::with_capacity(16);
    let mut at: *const Act = act;
    while !at.is_null() && stack.len() < depth {
        // Safety: every record on the chain is the frame of a loop still
        // running further up the native stack.
        let a = unsafe { &*at };
        stack.push(a.proc);
        at = a.parent as *const Act;
    }
    if let Some(stacks) = STACKS.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        *stacks.entry(stack).or_insert(0) += 1;
    }
}

/// The samples taken, as folded stacks, outermost first, most sampled
/// first, counted by name -- a definition's procedures, its methods and the
/// frames of its calls, are its own samples -- with a line about them for
/// the terminal.
pub fn finish() -> (String, String) {
    let stacks = STACKS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .unwrap_or_default();
    let v = super::vm();
    let mut names: HashMap<u32, String> = HashMap::new();
    let mut counts: HashMap<String, u64> = HashMap::new();
    let mut taken = 0;
    for (stack, n) in stacks {
        taken += n;
        let mut folded = String::new();
        for p in stack.iter().rev() {
            if !folded.is_empty() {
                folded.push(';');
            }
            let name = names.entry(*p).or_insert_with(|| {
                let full = &v.proc(*p).name;
                full.split('/').next().unwrap_or(full).to_string()
            });
            folded.push_str(name);
        }
        *counts.entry(folded).or_insert(0) += n;
    }
    let mut lines: Vec<(u64, String)> = counts.into_iter().map(|(s, n)| (n, s)).collect();
    lines.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = String::new();
    for (n, stack) in lines {
        out.push_str(&stack);
        out.push(' ');
        out.push_str(&n.to_string());
        out.push('\n');
    }
    let kind = if EVERY.load(Ordering::Relaxed) == 0 {
        "time"
    } else {
        "work"
    };
    let said = if taken == 0 {
        "profile: no samples -- the program did not run long enough".to_string()
    } else {
        format!("profile: {taken} samples of {kind}")
    };
    (out, said)
}
