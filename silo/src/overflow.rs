//! What a program says when it runs out of stack on Windows.
//!
//! Rust's own runtime prints "thread has overflowed its stack" from a handler
//! that its `main` installs; a program of Silo's has a `main` of its own, in
//! C, so that handler is never there, and a stack overflow ends the process
//! with `0xC00000FD` and not a word. This handler says it happened, on which
//! stack -- how deep it had got and how much it was given, which tells the
//! main thread's gigabyte from a green thread's or a segment's -- and with
//! `MEADOW_SILO_OVERFLOW_TRACE` set, the words on the stack that point into
//! the executable, as offsets in it: the return addresses of the frames it
//! holds, among other things, which a linker's map turns into names. With it
//! set, an access violation is reported the same way, where it happened and
//! what was being read or written: the first chance, before any handler of
//! the program's own has seen it, which Silo has none of.
//!
//! It runs on the stack that overflowed, in what Windows keeps back for that
//! (the thread's stack guarantee), so it formats into a buffer of its own and
//! writes it with one call, allocating nothing.

#[cfg(all(windows, target_arch = "x86_64"))]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[repr(C)]
    struct Record {
        code: u32,
        flags: u32,
        next: *mut Record,
        address: *mut std::ffi::c_void,
        count: u32,
        info: [usize; 15],
    }

    #[repr(C)]
    struct Pointers {
        record: *mut Record,
        context: *mut u8,
    }

    unsafe extern "system" {
        fn AddVectoredExceptionHandler(
            first: u32,
            handler: extern "system" fn(*mut std::ffi::c_void) -> i32,
        ) -> *mut std::ffi::c_void;
        fn GetModuleHandleW(name: *const u16) -> *mut u8;
        fn GetStdHandle(which: u32) -> *mut std::ffi::c_void;
        fn WriteFile(
            file: *mut std::ffi::c_void,
            buffer: *const u8,
            len: u32,
            written: *mut u32,
            overlapped: *mut std::ffi::c_void,
        ) -> i32;
    }

    const STACK_OVERFLOW: u32 = 0xc000_00fd;
    const ACCESS_VIOLATION: u32 = 0xc000_0005;
    const STD_ERROR: u32 = -12i32 as u32;
    /// Where `CONTEXT` keeps `Rsp` and `Rip` on x86-64.
    const RSP: usize = 0x98;
    const RIP: usize = 0xf8;
    /// How many candidates are written, and how far up the stack they are
    /// looked for.
    const SHOWN: usize = 400;
    const SCANNED: usize = 4 << 20;

    static SAID: AtomicBool = AtomicBool::new(false);
    /// `MEADOW_SILO_OVERFLOW_TRACE`, read once at the start: the handler
    /// allocates nothing.
    static TRACE: AtomicBool = AtomicBool::new(false);

    struct Out {
        buf: [u8; 16 << 10],
        len: usize,
    }

    static mut OUT: Out = Out {
        buf: [0; 16 << 10],
        len: 0,
    };

    impl Out {
        fn text(&mut self, s: &[u8]) {
            for b in s {
                if self.len < self.buf.len() {
                    self.buf[self.len] = *b;
                    self.len += 1;
                }
            }
        }

        fn hex(&mut self, mut n: usize) {
            let mut digits = [0u8; 16];
            let mut i = digits.len();
            loop {
                i -= 1;
                digits[i] = b"0123456789abcdef"[n & 15];
                n >>= 4;
                if n == 0 {
                    break;
                }
            }
            self.text(b"0x");
            self.text(&digits[i..]);
        }

        fn dec(&mut self, mut n: usize) {
            let mut digits = [0u8; 20];
            let mut i = digits.len();
            loop {
                i -= 1;
                digits[i] = b'0' + (n % 10) as u8;
                n /= 10;
                if n == 0 {
                    break;
                }
            }
            self.text(&digits[i..]);
        }

        fn flush(&mut self) {
            let mut written = 0u32;
            // Safety: the process's standard error, and a buffer of ours.
            unsafe {
                WriteFile(
                    GetStdHandle(STD_ERROR),
                    self.buf.as_ptr(),
                    self.len as u32,
                    &mut written,
                    std::ptr::null_mut(),
                );
            }
            self.len = 0;
        }
    }

    /// The running thread's stack, from its thread information block: where
    /// it begins (the top) and the bottom of what was reserved for it.
    fn stack() -> (usize, usize) {
        let teb: usize;
        // Safety: `gs:[0x30]` is the thread information block's own address.
        unsafe { std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb) };
        // Safety: `StackBase` and `DeallocationStack` of the running thread's
        // block, which corosensei keeps in step on a segment's stack too.
        unsafe {
            (
                *((teb + 0x08) as *const usize),
                *((teb + 0x1478) as *const usize),
            )
        }
    }

    extern "system" fn said(p: *mut std::ffi::c_void) -> i32 {
        let p = p as *mut Pointers;
        // Safety: what Windows hands a vectored handler.
        let (record, context) = unsafe { (&*(*p).record, (*p).context) };
        let traced = TRACE.load(Ordering::Relaxed);
        let fault = record.code == ACCESS_VIOLATION && traced;
        if (record.code != STACK_OVERFLOW && !fault) || SAID.swap(true, Ordering::SeqCst) {
            return 0;
        }
        // Safety: the context of the thread that overflowed, this one.
        let (sp, ip) = unsafe {
            (
                *(context.add(RSP) as *const usize),
                *(context.add(RIP) as *const usize),
            )
        };
        // Safety: the executable's own headers: `e_lfanew`, then
        // `SizeOfImage` in the optional header.
        let (image, size) = unsafe {
            let base = GetModuleHandleW(std::ptr::null());
            let nt = base.add(*(base.add(0x3c) as *const u32) as usize);
            (base as usize, *(nt.add(0x50) as *const u32) as usize)
        };
        let (top, bottom) = stack();
        // Safety: nothing else writes it -- `SAID` lets one thread in.
        let out = unsafe { &mut *std::ptr::addr_of_mut!(OUT) };
        if fault {
            out.text(b"\nsilo: an access violation, ");
            out.text(if record.info[0] == 1 {
                b"writing "
            } else {
                b"reading "
            });
            out.hex(record.info[1]);
            out.text(b", ");
        } else {
            out.text(b"\nsilo: the program overflowed its stack: ");
        }
        out.dec(top.saturating_sub(sp) >> 20);
        out.text(b" MiB deep in a stack of ");
        out.dec(top.saturating_sub(bottom) >> 20);
        out.text(b" MiB, at offset ");
        out.hex(ip.wrapping_sub(image));
        out.text(b" in the executable\n");
        if traced {
            out.text(
                b"silo: words on the stack that point into the executable, from the top frame:\n",
            );
            let mut shown = 0;
            let mut at = sp & !7;
            let end = top.min(sp.saturating_add(SCANNED));
            while at < end && shown < SHOWN {
                // Safety: committed stack, between the stack pointer and the
                // stack's top.
                let w = unsafe { *(at as *const usize) };
                if w > image && w < image + size {
                    out.hex(w - image);
                    out.text(b"\n");
                    shown += 1;
                    if out.len > out.buf.len() - 64 {
                        out.flush();
                    }
                }
                at += 8;
            }
            // And the outermost frames, nearest the stack's top: where the
            // recursion started.
            out.text(b"silo: ... and nearest the stack's top:\n");
            let mut at = top.saturating_sub(64 << 10).max(sp) & !7;
            while at < top {
                // Safety: as above.
                let w = unsafe { *(at as *const usize) };
                if w > image && w < image + size {
                    out.hex(w - image);
                    out.text(b"\n");
                    if out.len > out.buf.len() - 64 {
                        out.flush();
                    }
                }
                at += 8;
            }
        }
        out.flush();
        0
    }

    pub fn catch() {
        TRACE.store(
            std::env::var_os("MEADOW_SILO_OVERFLOW_TRACE").is_some(),
            Ordering::Relaxed,
        );
        // Safety: a handler that lives as long as the process.
        unsafe { AddVectoredExceptionHandler(1, said) };
    }
}

/// Say so when the program overflows a stack: see the module's comment.
pub fn catch() {
    #[cfg(all(windows, target_arch = "x86_64"))]
    imp::catch();
}
