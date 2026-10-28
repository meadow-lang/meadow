//! **What a program's memory did**, said when it ends: `meadow run
//! --gc-stats`, by way of `MEADOW_SILO_GC_STATS`, on either engine -- native
//! code or the VM -- and under either strategy.
//!
//! Traced, it is the collector's: how many collections, how long they kept
//! the program waiting, in all and at most, and the heap at the end.
//! Counted, it is what the counting did: the blocks acquired, and what the
//! cycle collector found and how long it took.

use std::sync::atomic::{AtomicU64, Ordering};

/// Whether the figures are wanted.
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MEADOW_SILO_GC_STATS").is_some())
}

/// Every thread's traced collections: how many, nanoseconds in all, and
/// the longest.
static COLLECTIONS: AtomicU64 = AtomicU64::new(0);
static NANOS: AtomicU64 = AtomicU64::new(0);
static LONGEST: AtomicU64 = AtomicU64::new(0);

/// A traced collection, which took `nanos`.
pub fn collected(nanos: u64) {
    COLLECTIONS.fetch_add(1, Ordering::Relaxed);
    NANOS.fetch_add(nanos, Ordering::Relaxed);
    LONGEST.fetch_max(nanos, Ordering::Relaxed);
}

fn bytes(words: u64) -> String {
    let b = words * 8;
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// Say the figures, if they are wanted: the program ran for `run`.
pub fn report(run: std::time::Duration) {
    if !on() {
        return;
    }
    let ms = |n: u64| n as f64 / 1e6;
    let run_ns = run.as_nanos().max(1) as u64;
    let share = |n: u64| 100.0 * n as f64 / run_ns as f64;
    if crate::trace::on() {
        let (n, nanos) = (
            COLLECTIONS.load(Ordering::Relaxed),
            NANOS.load(Ordering::Relaxed),
        );
        eprintln!(
            "gc: traced: {n} collection{}, {:.1} ms paused ({:.1}% of {:.1} ms), longest {:.2} ms",
            if n == 1 { "" } else { "s" },
            ms(nanos),
            share(nanos),
            ms(run_ns),
            ms(LONGEST.load(Ordering::Relaxed)),
        );
    } else {
        let t = crate::cycles::tally();
        eprintln!(
            "gc: counted: {} blocks acquired on the main thread",
            crate::heap::acquired()
        );
        eprintln!(
            "gc: {} cycle collection{}, {} blocks freed, {:.1} ms ({:.1}% of {:.1} ms), longest {:.2} ms",
            t.runs,
            if t.runs == 1 { "" } else { "s" },
            t.freed,
            ms(t.nanos),
            share(t.nanos),
            ms(run_ns),
            ms(t.longest),
        );
    }
    eprintln!(
        "gc: heaps {} at the end",
        bytes(crate::memory::HEAPS.load(Ordering::Relaxed) as u64)
    );
}
