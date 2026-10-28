//! **Executable memory**, for machine code made while a program runs: Glade's
//! JIT's, and the merged runtime's VM's (`docs/RUNTIME-MERGE.md`, "A JIT: the
//! design"). It was Glade's (`glade/src/jit.rs`), and is here so that both
//! can use it.
//!
//! One rule: no address is ever both writable and executable. On macOS it is
//! `MAP_JIT` memory, writable only by the thread writing it, for as long as it
//! writes -- other threads go on running what is there -- and on Apple silicon
//! the processor's cached copy of the instructions is told they changed.
//! Elsewhere each chunk is mapped twice from the same memory, a view to write
//! and a view to run: a `memfd` on Linux, a section backed by the page file on
//! Windows. See [`Arena`] for why functions are packed.

use std::ffi::c_void;

/// Executable memory, handed out a function at a time.
///
/// Functions are packed one after another into chunks, as a linker would lay
/// them out. Giving each its own mapping instead starts every one on a page --
/// on Windows a 64 KiB boundary -- so every function's entry has the same low
/// address bits and they all compete for the same few ways of the instruction
/// cache and the branch predictor, besides taking a page of the instruction TLB
/// each. On closure-heavy code that made the JIT half again slower than the
/// same translation ahead of time.
///
/// Memory is never writable and executable at one address. On macOS a
/// `MAP_JIT` mapping is writable only by the thread writing it, for as long as
/// it writes. Elsewhere each chunk is mapped twice, from the same memory: a
/// view to write through and a view to run, which never changes. Where the
/// system will not make the pair, each function gets pages of its own after
/// all, written and then made executable.
#[derive(Default)]
pub struct Arena {
    /// Every single mapping made: `(start, length)`.
    maps: Vec<(*mut u8, usize)>,
    /// How far into the last chunk it is filled.
    free: usize,
    /// Chunks mapped twice, the last one being filled.
    #[cfg(not(target_os = "macos"))]
    views: Vec<Views>,
    /// The system would not map a chunk twice: stop asking.
    #[cfg(not(target_os = "macos"))]
    alone: bool,
}

/// One chunk's two views of the same memory.
#[cfg(not(target_os = "macos"))]
struct Views {
    write: *mut u8,
    exec: *mut u8,
    len: usize,
}

// The mappings are only written under the arena's lock, and only read and run
// elsewhere.
unsafe impl Send for Arena {}

impl Drop for Arena {
    fn drop(&mut self) {
        for &(p, len) in &self.maps {
            // Safety: mappings this arena made, which nothing runs once the
            // `Native` holding it is gone.
            unsafe {
                unmap(p, len);
            }
        }
        #[cfg(not(target_os = "macos"))]
        for v in &self.views {
            // Safety: as above.
            unsafe {
                unmap_views(v);
            }
        }
    }
}

/// Give back a mapping [`Arena`] made.
#[cfg(unix)]
unsafe fn unmap(p: *mut u8, len: usize) {
    // Safety: the caller's.
    unsafe {
        munmap(p as *mut c_void, len);
    }
}

#[cfg(windows)]
unsafe fn unmap(p: *mut u8, _len: usize) {
    // Safety: the caller's. `MEM_RELEASE` takes the whole reservation, and
    // wants a size of zero to do it.
    unsafe {
        VirtualFree(p as *mut c_void, 0, MEM_RELEASE);
    }
}

/// How much a chunk holds, unless one function needs more.
const CHUNK: usize = 1 << 20;

impl Arena {
    /// `code`, somewhere executable, starting at a multiple of `align`
    /// bytes: where it starts.
    #[cfg(target_os = "macos")]
    pub fn put(&mut self, code: &[u8], align: usize) -> Option<*mut u8> {
        let offset = self.free.div_ceil(align) * align;
        let fits = self
            .maps
            .last()
            .is_some_and(|&(_, len)| offset + code.len() <= len);
        let offset = if fits {
            offset
        } else {
            let len = code.len().max(CHUNK);
            // Safety: a fresh anonymous mapping.
            let p = unsafe { map(len, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_JIT) }?;
            self.maps.push((p, len));
            0
        };
        let &(base, _) = self.maps.last().expect("a mapping");
        // Safety: `offset + code.len()` is within the mapping, and nothing runs
        // that part of it yet.
        let at = unsafe { base.add(offset) };
        unsafe {
            write_protect(false);
            std::ptr::copy_nonoverlapping(code.as_ptr(), at, code.len());
            write_protect(true);
            sys_icache_invalidate(at as *mut c_void, code.len());
        }
        self.free = offset + code.len();
        Some(at)
    }

    /// `code`, somewhere executable, starting at a multiple of `align`
    /// bytes: where it starts. Packed into a chunk mapped twice if it can be,
    /// on pages of its own if not.
    #[cfg(not(target_os = "macos"))]
    pub fn put(&mut self, code: &[u8], align: usize) -> Option<*mut u8> {
        if !self.alone {
            match self.packed(code, align) {
                Some(at) => return Some(at),
                None => self.alone = true,
            }
        }
        self.put_alone(code)
    }

    #[cfg(not(target_os = "macos"))]
    fn packed(&mut self, code: &[u8], align: usize) -> Option<*mut u8> {
        let offset = self.free.div_ceil(align) * align;
        let fits = self
            .views
            .last()
            .is_some_and(|v| offset + code.len() <= v.len);
        let offset = if fits {
            offset
        } else {
            // Whole 64 KiB, the granularity Windows maps views at.
            let len = code.len().max(CHUNK).div_ceil(1 << 16) << 16;
            // Safety: a fresh pair of views.
            let v = unsafe { views(len) }?;
            self.views.push(v);
            0
        };
        let v = self.views.last().expect("a chunk");
        // Safety: `offset + code.len()` is within the chunk, and nothing runs
        // that part of it yet: it is published only once this returns.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), v.write.add(offset), code.len());
            let at = v.exec.add(offset);
            flush(at, code.len());
            self.free = offset + code.len();
            Some(at)
        }
    }

    /// `code` on pages of its own, written and then made executable.
    #[cfg(windows)]
    fn put_alone(&mut self, code: &[u8]) -> Option<*mut u8> {
        let page = 4096;
        let len = code.len().max(1).div_ceil(page) * page;
        // Safety: fresh pages, written and then made executable before anyone
        // is told where they are.
        unsafe {
            let p = VirtualAlloc(
                std::ptr::null_mut(),
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            ) as *mut u8;
            if p.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(code.as_ptr(), p, code.len());
            let mut old = 0u32;
            if VirtualProtect(p as *mut c_void, len, PAGE_EXECUTE_READ, &mut old) == 0 {
                VirtualFree(p as *mut c_void, 0, MEM_RELEASE);
                return None;
            }
            FlushInstructionCache(GetCurrentProcess(), p as *const c_void, code.len());
            self.maps.push((p, len));
            Some(p)
        }
    }

    /// `code` on pages of its own, written and then made executable.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn put_alone(&mut self, code: &[u8]) -> Option<*mut u8> {
        let page = 4096;
        let len = code.len().max(1).div_ceil(page) * page;
        // Safety: a fresh anonymous mapping, written and then made executable
        // before anyone is told where it is.
        unsafe {
            let p = map(len, PROT_READ | PROT_WRITE, 0)?;
            std::ptr::copy_nonoverlapping(code.as_ptr(), p, code.len());
            if mprotect(p as *mut c_void, len, PROT_READ | PROT_EXEC) != 0 {
                munmap(p as *mut c_void, len);
                return None;
            }
            #[cfg(target_arch = "aarch64")]
            __clear_cache(p as *mut c_void, p.add(code.len()) as *mut c_void);
            self.maps.push((p, len));
            Some(p)
        }
    }
}

/// `len` bytes of fresh memory, mapped twice: once writable, once executable.
#[cfg(windows)]
unsafe fn views(len: usize) -> Option<Views> {
    // Safety: a pagefile-backed section of our own, and views of all of it.
    unsafe {
        let h = CreateFileMappingW(
            -1isize as *mut c_void,
            std::ptr::null_mut(),
            PAGE_EXECUTE_READWRITE,
            (len >> 32) as u32,
            len as u32,
            std::ptr::null(),
        );
        if h.is_null() {
            return None;
        }
        let write = MapViewOfFile(h, FILE_MAP_WRITE, 0, 0, len) as *mut u8;
        let exec = MapViewOfFile(h, FILE_MAP_READ | FILE_MAP_EXECUTE, 0, 0, len) as *mut u8;
        // The views keep the section alive.
        CloseHandle(h);
        let v = Views { write, exec, len };
        if write.is_null() || exec.is_null() {
            unmap_views(&v);
            return None;
        }
        Some(v)
    }
}

#[cfg(windows)]
unsafe fn unmap_views(v: &Views) {
    // Safety: the caller's.
    unsafe {
        for p in [v.write, v.exec] {
            if !p.is_null() {
                UnmapViewOfFile(p as *const c_void);
            }
        }
    }
}

/// Code just written at `at`, through the other view, is what runs there.
#[cfg(windows)]
unsafe fn flush(at: *mut u8, len: usize) {
    // Safety: a range of a view this process mapped.
    unsafe {
        FlushInstructionCache(GetCurrentProcess(), at as *const c_void, len);
    }
}

#[cfg(windows)]
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
#[cfg(windows)]
const FILE_MAP_WRITE: u32 = 0x0002;
#[cfg(windows)]
const FILE_MAP_READ: u32 = 0x0004;
#[cfg(windows)]
const FILE_MAP_EXECUTE: u32 = 0x0020;

/// `len` bytes of fresh memory, mapped twice: once writable, once executable.
#[cfg(all(unix, not(target_os = "macos")))]
unsafe fn views(len: usize) -> Option<Views> {
    // Safety: an anonymous file of our own, and shared mappings of all of it.
    unsafe {
        let fd = memfd_create(c"meadow-jit".as_ptr(), MFD_CLOEXEC);
        if fd < 0 {
            return None;
        }
        if ftruncate(fd, len as i64) != 0 {
            close(fd);
            return None;
        }
        let at = |prot| {
            let p = mmap(std::ptr::null_mut(), len, prot, MAP_SHARED, fd, 0);
            if p as isize == -1 {
                std::ptr::null_mut()
            } else {
                p as *mut u8
            }
        };
        let write = at(PROT_READ | PROT_WRITE);
        let exec = at(PROT_READ | PROT_EXEC);
        // The mappings keep the file alive.
        close(fd);
        let v = Views { write, exec, len };
        if write.is_null() || exec.is_null() {
            unmap_views(&v);
            return None;
        }
        Some(v)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
unsafe fn unmap_views(v: &Views) {
    // Safety: the caller's.
    unsafe {
        for p in [v.write, v.exec] {
            if !p.is_null() {
                munmap(p as *mut c_void, v.len);
            }
        }
    }
}

/// Code just written at `at`, through the other view, is what runs there.
#[cfg(all(unix, not(target_os = "macos")))]
unsafe fn flush(at: *mut u8, len: usize) {
    // Safety: a range of a mapping this process made.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        __clear_cache(at as *mut c_void, at.add(len) as *mut c_void);
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = (at, len);
}

#[cfg(all(unix, not(target_os = "macos")))]
const MAP_SHARED: i32 = 0x0001;
#[cfg(all(unix, not(target_os = "macos")))]
const MFD_CLOEXEC: u32 = 0x0001;

#[cfg(windows)]
const MEM_COMMIT: u32 = 0x1000;
#[cfg(windows)]
const MEM_RESERVE: u32 = 0x2000;
#[cfg(windows)]
const MEM_RELEASE: u32 = 0x8000;
#[cfg(windows)]
const PAGE_READWRITE: u32 = 0x04;
#[cfg(windows)]
const PAGE_EXECUTE_READ: u32 = 0x20;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualProtect(addr: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
    fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
    fn FlushInstructionCache(process: *mut c_void, base: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn CreateFileMappingW(
        file: *mut c_void,
        attributes: *mut c_void,
        protect: u32,
        size_high: u32,
        size_low: u32,
        name: *const u16,
    ) -> *mut c_void;
    fn MapViewOfFile(
        mapping: *mut c_void,
        access: u32,
        offset_high: u32,
        offset_low: u32,
        size: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(base: *const c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

#[cfg(unix)]
const PROT_READ: i32 = 1;
#[cfg(unix)]
const PROT_WRITE: i32 = 2;
#[cfg(unix)]
const PROT_EXEC: i32 = 4;
#[cfg(unix)]
const MAP_PRIVATE: i32 = 0x0002;
#[cfg(target_os = "macos")]
const MAP_ANON: i32 = 0x1000;
#[cfg(all(unix, not(target_os = "macos")))]
const MAP_ANON: i32 = 0x20;
#[cfg(target_os = "macos")]
const MAP_JIT: i32 = 0x0800;

#[cfg(unix)]
unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
    -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    #[cfg(all(unix, not(target_os = "macos")))]
    fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
    fn memfd_create(name: *const std::ffi::c_char, flags: u32) -> i32;
    #[cfg(target_os = "android")]
    fn syscall(number: std::ffi::c_long, ...) -> std::ffi::c_long;
    #[cfg(all(unix, not(target_os = "macos")))]
    fn ftruncate(fd: i32, len: i64) -> i32;
    #[cfg(all(unix, not(target_os = "macos")))]
    fn close(fd: i32) -> i32;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn pthread_jit_write_protect_np(enabled: i32);
    #[cfg(target_os = "macos")]
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
    #[cfg(all(
        unix,
        not(target_os = "macos"),
        not(target_os = "android"),
        target_arch = "aarch64"
    ))]
    fn __clear_cache(start: *mut c_void, end: *mut c_void);
}

/// `memfd_create`, which Android's C library has only from Android 11 (API
/// 30). An Android build links against an older one -- Termux runs on Android
/// 7 -- so this asks the kernel directly, which has had the call since Linux
/// 3.17. A kernel older still answers -1, and so does anything that refuses
/// it; either way `views` fails, and the JIT puts code on pages of its own.
#[cfg(target_os = "android")]
unsafe fn memfd_create(name: *const std::ffi::c_char, flags: u32) -> i32 {
    let number: std::ffi::c_long = if cfg!(target_arch = "aarch64") {
        279
    } else if cfg!(target_arch = "x86_64") {
        319
    } else {
        return -1;
    };
    // Safety: the call's own arguments, as the kernel takes them.
    unsafe { syscall(number, name, flags) as i32 }
}

/// What `__clear_cache` does on ARM64, written out. On Linux it is libgcc's,
/// and Android links no library that has it -- it is a few instructions.
///
/// Code written through one view and run through another is only seen as
/// written once the data cache has been cleaned to where the instruction side
/// reads from, and the instruction cache invalidated over it: each a line at a
/// time, by the line sizes `CTR_EL0` gives. A core that says it keeps the two
/// coherent itself -- `IDC`, `DIC` -- skips the step it does not need.
#[cfg(all(target_os = "android", target_arch = "aarch64"))]
unsafe fn __clear_cache(start: *mut c_void, end: *mut c_void) {
    use std::arch::asm;
    let (start, end) = (start as usize, end as usize);
    let ctr: u64;
    // Safety: a register the kernel lets user code read, on every ARM64 Linux.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    if (ctr >> 28) & 1 == 0 {
        let line = 4usize << ((ctr >> 16) & 15);
        let mut at = start & !(line - 1);
        while at < end {
            // Safety: a line of a mapping this process made.
            unsafe { asm!("dc cvau, {}", in(reg) at, options(nostack, preserves_flags)) };
            at += line;
        }
    }
    // Safety: barriers, no operands.
    unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
    if (ctr >> 29) & 1 == 0 {
        let line = 4usize << (ctr & 15);
        let mut at = start & !(line - 1);
        while at < end {
            // Safety: as above.
            unsafe { asm!("ic ivau, {}", in(reg) at, options(nostack, preserves_flags)) };
            at += line;
        }
        // Safety: as above.
        unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
    }
    // Safety: as above.
    unsafe { asm!("isb sy", options(nostack, preserves_flags)) };
}

/// An anonymous private mapping of `len` bytes.
#[cfg(unix)]
unsafe fn map(len: usize, prot: i32, flags: i32) -> Option<*mut u8> {
    // Safety: asked for in full, and checked.
    let p = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            prot,
            MAP_PRIVATE | MAP_ANON | flags,
            -1,
            0,
        )
    };
    (p as isize != -1).then_some(p as *mut u8)
}

/// Make this thread's view of `MAP_JIT` memory writable, or executable again.
#[cfg(target_os = "macos")]
unsafe fn write_protect(on: bool) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        pthread_jit_write_protect_np(on as i32);
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = on;
}
