//! **A sampling profile of a native program, taken by the program.**
//!
//! `MEADOW_SILO_PROFILE=<file>` has the runtime interrupt itself a thousand
//! times for each second of processor time it uses, and write down where it
//! was: the return addresses on the stack, innermost first. At exit they go
//! to the file, a line a sample, after a line saying where the program was
//! loaded -- so that addresses can be put against the symbols `nm` lists of
//! the executable, which is what `scripts/silo-profile.py` does.
//!
//! It exists for where nothing outside the process may look: a container
//! that allows neither `perf` nor `ptrace` leaves a program no profiler but
//! itself. On macOS `sample` does this without being asked.
//!
//! The handler does as little as a handler can: it takes a slot in a table
//! that was there before the program started, and walks the stack into it.
//! Nothing is allocated and nothing is named until the program is over.

#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
mod imp {
    use std::ffi::{c_char, c_int, c_void};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// How many frames of a sample are kept, and how many samples: four
    /// minutes of one processor, after which no more are taken.
    const DEPTH: usize = 12;
    const SAMPLES: usize = 1 << 18;

    static mut TABLE: [usize; SAMPLES * DEPTH] = [0; SAMPLES * DEPTH];
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    const SIGPROF: c_int = 27;
    const ITIMER_PROF: c_int = 2;

    /// Two `timeval`s. A `timeval`'s microseconds are narrower than this on
    /// macOS, with padding after: a small number written wide is the same
    /// number there, the machine being little-endian.
    #[repr(C)]
    struct Timer {
        every_s: i64,
        every_us: i64,
        first_s: i64,
        first_us: i64,
    }

    #[repr(C)]
    struct Where {
        file: *const c_char,
        base: *mut c_void,
        symbol: *const c_char,
        at: *mut c_void,
    }

    unsafe extern "C" {
        fn signal(sig: c_int, handler: usize) -> usize;
        fn setitimer(which: c_int, new: *const Timer, old: *mut Timer) -> c_int;
        fn backtrace(into: *mut *mut c_void, size: c_int) -> c_int;
        fn dladdr(addr: *const c_void, info: *mut Where) -> c_int;
    }

    extern "C" fn tick(_: c_int) {
        let i = NEXT.fetch_add(1, Ordering::Relaxed);
        if i >= SAMPLES {
            return;
        }
        // This frame and the one the signal arrived through come first, and
        // are not the program's.
        let mut frames = [std::ptr::null_mut::<c_void>(); DEPTH + 2];
        // Safety: room for as many as it is told of.
        let n = unsafe { backtrace(frames.as_mut_ptr(), (DEPTH + 2) as c_int) } as usize;
        for k in 2..n.min(DEPTH + 2) {
            // Safety: slot `i` is this handler's alone, and inside the table.
            unsafe {
                (&raw mut TABLE)
                    .cast::<usize>()
                    .add(i * DEPTH + (k - 2))
                    .write(frames[k] as usize);
            }
        }
    }

    pub fn start() {
        if std::env::var_os("MEADOW_SILO_PROFILE").is_none() {
            return;
        }
        // The unwinder is loaded the first time it is used, which must not
        // be inside the handler.
        let mut warm = [std::ptr::null_mut::<c_void>(); 2];
        let every = Timer {
            every_s: 0,
            every_us: 1000,
            first_s: 0,
            first_us: 1000,
        };
        // Safety: a handler that touches only its table, and a timer.
        unsafe {
            backtrace(warm.as_mut_ptr(), 2);
            signal(SIGPROF, tick as *const () as usize);
            setitimer(ITIMER_PROF, &every, std::ptr::null_mut());
        }
    }

    pub fn finish() {
        let Some(path) = std::env::var_os("MEADOW_SILO_PROFILE") else {
            return;
        };
        let off = Timer {
            every_s: 0,
            every_us: 0,
            first_s: 0,
            first_us: 0,
        };
        // Safety: the timer stopped, so the table is no longer written.
        unsafe { setitimer(ITIMER_PROF, &off, std::ptr::null_mut()) };
        let mut at = Where {
            file: std::ptr::null(),
            base: std::ptr::null_mut(),
            symbol: std::ptr::null(),
            at: std::ptr::null_mut(),
        };
        // Safety: an address in this program, and room for the answer.
        unsafe { dladdr(finish as *const c_void, &mut at) };
        let taken = NEXT.load(Ordering::Relaxed).min(SAMPLES);
        let mut out = format!("base {:x}\n", at.base as usize);
        for i in 0..taken {
            let mut line = String::new();
            for k in 0..DEPTH {
                // Safety: inside the table, and nothing writes it now.
                let a = unsafe { (&raw const TABLE).cast::<usize>().add(i * DEPTH + k).read() };
                if a == 0 {
                    break;
                }
                if k > 0 {
                    line.push(' ');
                }
                line.push_str(&format!("{a:x}"));
            }
            out.push_str(&line);
            out.push('\n');
        }
        if let Err(e) = std::fs::write(&path, out) {
            eprintln!("aot: the profile could not be written: {e}");
        }
    }
}

#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
mod imp {
    pub fn start() {}
    pub fn finish() {}
}

/// Begin sampling, if `MEADOW_SILO_PROFILE` names a file to write to.
pub fn start() {
    imp::start();
}

/// Stop, and write what was sampled.
pub fn finish() {
    imp::finish();
}
