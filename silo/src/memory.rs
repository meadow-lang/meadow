//! **What a native program holds, as it runs.**
//!
//! `MEADOW_SILO_MEMORY=1` has the runtime say on stderr, four times a second,
//! how much its threads' heaps have taken from the system, how much of that is
//! in blocks the program still has, and how much its compact regions hold --
//! each line with the milliseconds since the program started, to set against
//! whatever else the program says of where it has got to. A process's
//! resident memory says how much; this says of what, and whether it is in use.
//!
//! The first and last are counted always, a chunk at a time, which costs
//! nothing. The blocks in use are counted only when asked, since that is an
//! addition for every block made and let go of.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Words the heaps have from the system: their chunks, and the blocks too big
/// for one.
pub static HEAPS: AtomicUsize = AtomicUsize::new(0);
/// Words in blocks made and not yet cleaned, when [`on`].
pub static LIVE: AtomicUsize = AtomicUsize::new(0);
/// Words the regions have from the system.
pub static REGIONS: AtomicUsize = AtomicUsize::new(0);
static ON: AtomicBool = AtomicBool::new(false);

/// Whether the blocks in use are being counted.
#[inline]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Begin saying, if `MEADOW_SILO_MEMORY` is set.
pub fn start() {
    if std::env::var_os("MEADOW_SILO_MEMORY").is_none() {
        return;
    }
    ON.store(true, Ordering::Relaxed);
    let began = std::time::Instant::now();
    let mb = |words: usize| words * 8 / (1 << 20);
    let said = std::thread::Builder::new()
        .name("meadow memory".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                eprintln!(
                    "aot: mem {} ms: heaps {} MB, of it in use {} MB; regions {} MB in {}",
                    began.elapsed().as_millis(),
                    mb(HEAPS.load(Ordering::Relaxed)),
                    mb(LIVE.load(Ordering::Relaxed)),
                    mb(REGIONS.load(Ordering::Relaxed)),
                    crate::region::live(),
                );
            }
        });
    if said.is_err() {
        eprintln!("aot: the memory report could not be started");
    }
}
