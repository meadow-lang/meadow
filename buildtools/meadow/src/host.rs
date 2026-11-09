//! Which file this code was loaded from.
//!
//! `meadow` is a program, and it is also `meadow-host`, a shared library that
//! MeadowBoot loads to run what it does not do itself. Several things ask
//! where `meadow` is -- the runtime libraries kept beside it, and what names
//! this build in a cache, so that a rebuilt one never reads an older one's --
//! and in the library the executable is MeadowBoot's, not this. So they ask
//! here: the file that holds this code, executable or library.

use std::path::PathBuf;

/// The file this code is in, or the running executable where the system
/// cannot say.
pub fn this_build() -> Option<PathBuf> {
    static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| holder().or_else(|| std::env::current_exe().ok()))
        .clone()
}

/// The address of a function of this crate's, which is in whatever file
/// this crate was linked into.
fn here() -> *const std::ffi::c_void {
    this_build as fn() -> Option<PathBuf> as *const std::ffi::c_void
}

#[cfg(windows)]
fn holder() -> Option<PathBuf> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
        fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, size: u32) -> u32;
    }
    // Look the module up by an address in it, and leave its count alone.
    const FROM_ADDRESS: u32 = 0x4;
    const UNCHANGED_REFCOUNT: u32 = 0x2;
    let mut module = std::ptr::null_mut();
    // SAFETY: `here()` is an address in this module, and `module` is written
    // only on success.
    let found = unsafe {
        GetModuleHandleExW(
            FROM_ADDRESS | UNCHANGED_REFCOUNT,
            here().cast(),
            &mut module,
        )
    };
    if found == 0 {
        return None;
    }
    let mut name = vec![0u16; 1024];
    loop {
        // SAFETY: `name` has room for `len` units, and is written no further.
        let got = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) };
        if got == 0 {
            return None;
        }
        if (got as usize) < name.len() {
            name.truncate(got as usize);
            return Some(PathBuf::from(std::ffi::OsString::from_wide(&name)));
        }
        // Cut short: try again with more room.
        name.resize(name.len() * 2, 0);
    }
}

#[cfg(unix)]
fn holder() -> Option<PathBuf> {
    use std::ffi::{CStr, c_char, c_void};
    #[repr(C)]
    struct Info {
        fname: *const c_char,
        fbase: *mut c_void,
        sname: *const c_char,
        saddr: *mut c_void,
    }
    unsafe extern "C" {
        fn dladdr(address: *const c_void, info: *mut Info) -> i32;
    }
    let mut info = Info {
        fname: std::ptr::null(),
        fbase: std::ptr::null_mut(),
        sname: std::ptr::null(),
        saddr: std::ptr::null_mut(),
    };
    // SAFETY: `info` is a `Dl_info`, written only on success.
    if unsafe { dladdr(here(), &mut info) } == 0 || info.fname.is_null() {
        return None;
    }
    // SAFETY: on success `fname` is a NUL-terminated path the loader keeps.
    let name = unsafe { CStr::from_ptr(info.fname) }.to_str().ok()?;
    // The executable's own entry may be named as it was started, relative to
    // wherever that was; the executable is better asked by its own name.
    let path = std::fs::canonicalize(name).ok()?;
    let exe = std::env::current_exe()
        .ok()
        .and_then(|e| std::fs::canonicalize(e).ok());
    if exe.as_ref() == Some(&path) {
        exe
    } else {
        Some(path)
    }
}

#[cfg(not(any(windows, unix)))]
fn holder() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_its_own_build() {
        // A test is an executable, so the file is the executable.
        let exe = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let found = std::fs::canonicalize(this_build().expect("a file")).unwrap();
        assert_eq!(found, exe);
    }
}
