//! Calling C: what `Std.Ffi` asks of a runtime, the same for all three.
//!
//! A library is opened by name, a function found in it by name, and the
//! function called with integers, floats, pointers and strings, answering
//! one of those. There is no description of the function here to check the
//! call against: `Std.Ffi` says what the arguments are and what comes back,
//! and a call that says it wrongly is a C program's undefined behaviour.
//!
//! **How a call is made without knowing its type.** The C conventions of
//! aarch64 and x86-64 (System V) pass the first integers in one set of
//! registers and the first floats in another, each in order, and a function
//! reads only the registers of the parameters it has. So every function is
//! called as one of eight integers and eight floats -- the arguments in the
//! first of each, zeros in the rest -- and what it does not take it does not
//! look at. That holds for up to eight integers (six on x86-64) and eight
//! floats; past that an argument goes on the stack, in an order this cannot
//! arrange, and the call is refused. A struct by value and a variadic
//! function are refused by not being expressible.
//!
//! **Windows on x86-64** passes the first four arguments by place, not by
//! kind -- the second in `rdx` or `xmm1`, whichever its type says -- and the
//! rest on the stack, eight bytes each whatever they are. So a call there is
//! one of sixteen shapes, by which of the first four are floats, of sixteen
//! words: what is past the fourth is the same eight bytes either way, and a
//! function reads only the arguments it has.
//!
//! **A C `float`** is an `f32` in the low half of a float register, or of its
//! stack slot, and the high half is not looked at: one is passed as a word
//! whose low half is its bits, and one answered is the low half of what came
//! back.
//!
//! Memory is C's: [`alloc`] and [`free`] are `malloc` and `free`, and numbers
//! are written to and read from an address as so many bytes each. Nothing
//! here is checked, and nothing could be.
//!
//! Unix, and Windows on x86-64.

/// An argument, as the C function is to be handed it.
pub enum Arg {
    /// An integer or a pointer.
    Int(i64),
    /// A `double`.
    Float(f64),
    /// A `float`.
    Float32(f64),
    /// Text, as a NUL-terminated string that lasts as long as the call.
    Str(String),
}

/// What a call answers, as the C function's return type.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ret {
    Unit,
    Int,
    /// A `double`.
    Float,
    /// A `const char *`, copied; a null one is the empty string.
    Str,
    /// A `float`.
    Float32,
}

impl Ret {
    /// As `Std.Ffi` numbers them: 0 unit, 1 integer or pointer, 2 `double`,
    /// 3 string, 4 `float`.
    pub fn from_code(code: i64) -> Option<Ret> {
        Some(match code {
            0 => Ret::Unit,
            1 => Ret::Int,
            2 => Ret::Float,
            3 => Ret::Str,
            4 => Ret::Float32,
            _ => return None,
        })
    }

    /// Whether the answer comes back in a float register.
    fn in_float(self) -> bool {
        matches!(self, Ret::Float | Ret::Float32)
    }
}

/// An argument as the word it is passed as, and whether it goes in a float
/// register: a `float`'s bits in the low half of its word.
fn word(a: &Arg, texts: &mut Vec<std::ffi::CString>) -> Result<(u64, bool), String> {
    Ok(match a {
        Arg::Int(n) => (*n as u64, false),
        Arg::Float(x) => (x.to_bits(), true),
        Arg::Float32(x) => (u64::from((*x as f32).to_bits()), true),
        Arg::Str(s) => {
            let c = std::ffi::CString::new(s.as_str())
                .map_err(|_| "a NUL in a string argument".to_string())?;
            texts.push(c);
            (texts.last().expect("just pushed").as_ptr() as u64, false)
        }
    })
}

/// What came back -- the integer register, and the float register's bits --
/// as `ret` says to read it.
fn answer(ret: Ret, int: i64, float: u64) -> Answer {
    match ret {
        Ret::Unit => (0, 0.0, String::new()),
        Ret::Int => (int, 0.0, String::new()),
        Ret::Float => (0, f64::from_bits(float), String::new()),
        Ret::Float32 => (0, f64::from(f32::from_bits(float as u32)), String::new()),
        Ret::Str if int == 0 => (0, 0.0, String::new()),
        // Safety: a C string the function answered, the caller says.
        Ret::Str => (
            0,
            0.0,
            unsafe { std::ffi::CStr::from_ptr(int as *const std::ffi::c_char) }
                .to_string_lossy()
                .into_owned(),
        ),
    }
}

/// What came back: the integer, the float and the text, of which the call's
/// [`Ret`] says which one means anything.
pub type Answer = (i64, f64, String);

#[cfg(unix)]
mod sys {
    use super::{Answer, Arg, Ret};
    use std::ffi::{CStr, CString, c_void};

    /// How many integer arguments travel in registers.
    const INTS: usize = if cfg!(target_arch = "x86_64") { 6 } else { 8 };
    const FLOATS: usize = 8;

    fn last_error() -> String {
        // Safety: `dlerror` answers a string of its own, or null.
        unsafe {
            let e = libc::dlerror();
            if e.is_null() {
                "unknown error".to_string()
            } else {
                CStr::from_ptr(e).to_string_lossy().into_owned()
            }
        }
    }

    pub fn open(path: &str) -> Result<i64, String> {
        if path.is_empty() {
            return own();
        }
        let name = CString::new(path).map_err(|_| "a NUL in the library's name".to_string())?;
        // Safety: a C string, and flags `dlopen` documents.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            Err(last_error())
        } else {
            Ok(handle as i64)
        }
    }

    /// The library this code is linked into -- a program's own executable,
    /// or the library a plugin is -- found by an address in it.
    fn own() -> Result<i64, String> {
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        // Safety: an address in this module, and room for the answer.
        let found = unsafe { libc::dladdr(own as *const c_void, &mut info) };
        if found == 0 || info.dli_fname.is_null() {
            return Err("this library's own path could not be found".to_string());
        }
        // Safety: the path the loader keeps for the module, and the flags
        // `dlopen` documents: the module is loaded already.
        let handle = unsafe { libc::dlopen(info.dli_fname, libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            Err(last_error())
        } else {
            Ok(handle as i64)
        }
    }

    pub fn symbol(library: i64, name: &str) -> Result<i64, String> {
        let c = CString::new(name).map_err(|_| "a NUL in the function's name".to_string())?;
        // Safety: a handle `open` answered, and a C string.
        let at = unsafe { libc::dlsym(library as *mut c_void, c.as_ptr()) };
        if at.is_null() {
            Err(format!("no `{name}`: {}", last_error()))
        } else {
            Ok(at as i64)
        }
    }

    pub fn call(function: i64, ret: Ret, args: &[Arg]) -> Result<Answer, String> {
        if function == 0 {
            return Err("a call to a null function".to_string());
        }
        let mut ints = [0i64; 8];
        let mut floats = [0f64; 8];
        let (mut ni, mut nf) = (0, 0);
        // The strings, kept until the call returns.
        let mut texts: Vec<CString> = Vec::new();
        for a in args {
            let (w, float) = super::word(a, &mut texts)?;
            if float {
                if nf == FLOATS {
                    return Err(format!("more than {FLOATS} float arguments"));
                }
                floats[nf] = f64::from_bits(w);
                nf += 1;
            } else {
                if ni == INTS {
                    return Err(format!(
                        "more than {INTS} integer, pointer and string arguments"
                    ));
                }
                ints[ni] = w as i64;
                ni += 1;
            }
        }
        type Ints = extern "C" fn(
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
        ) -> i64;
        type Floats = extern "C" fn(
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
        ) -> f64;
        let [i0, i1, i2, i3, i4, i5, i6, i7] = ints;
        let [f0, f1, f2, f3, f4, f5, f6, f7] = floats;
        // Safety: none that can be checked. The address is one `symbol`
        // answered; that it is a function of these arguments answering this
        // is the caller's word. See the module's note on why the extra
        // registers are harmless.
        let (int, float) = unsafe {
            if ret.in_float() {
                let f: Floats = std::mem::transmute(function as *const c_void);
                let x = f(
                    i0, i1, i2, i3, i4, i5, i6, i7, f0, f1, f2, f3, f4, f5, f6, f7,
                );
                (0, x.to_bits())
            } else {
                let f: Ints = std::mem::transmute(function as *const c_void);
                let n = f(
                    i0, i1, i2, i3, i4, i5, i6, i7, f0, f1, f2, f3, f4, f5, f6, f7,
                );
                (n, 0)
            }
        };
        let answered = super::answer(ret, int, float);
        drop(texts);
        Ok(answered)
    }

    pub fn alloc(bytes: i64) -> i64 {
        // Safety: `malloc`, which answers null when it cannot.
        unsafe { libc::calloc(1, bytes.max(1) as usize) as i64 }
    }

    pub fn free(at: i64) {
        // Safety: an address `alloc` or C answered, the caller says.
        unsafe { libc::free(at as *mut c_void) }
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
mod sys {
    use super::{Answer, Arg, Ret};
    use std::ffi::{CString, c_char, c_void};

    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        fn GetLastError() -> u32;
    }

    unsafe extern "C" {
        fn calloc(count: usize, size: usize) -> *mut c_void;
        #[link_name = "free"]
        fn c_free(at: *mut c_void);
    }

    /// How many words a call passes: four in registers and twelve on the
    /// stack.
    const WORDS: usize = 16;

    pub fn open(path: &str) -> Result<i64, String> {
        if path.is_empty() {
            // The module this code is linked into -- a program's own
            // executable, or the library a plugin is -- by an address in it,
            // and not counted as one more load of it.
            const FROM_ADDRESS: u32 = 0x4;
            const UNCHANGED_REFCOUNT: u32 = 0x2;
            let mut module: *mut c_void = std::ptr::null_mut();
            // Safety: an address in this module, and room for the answer.
            let ok = unsafe {
                GetModuleHandleExW(
                    FROM_ADDRESS | UNCHANGED_REFCOUNT,
                    open as *const () as *const u16,
                    &mut module,
                )
            };
            return if ok == 0 || module.is_null() {
                // Safety: none needed.
                Err(format!(
                    "this library could not be found: Windows error {}",
                    unsafe { GetLastError() }
                ))
            } else {
                Ok(module as i64)
            };
        }
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        // Safety: a NUL-terminated wide string.
        let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
        if handle.is_null() {
            // Safety: none needed.
            Err(format!(
                "`{path}` could not be loaded: Windows error {}",
                unsafe { GetLastError() }
            ))
        } else {
            Ok(handle as i64)
        }
    }

    pub fn symbol(library: i64, name: &str) -> Result<i64, String> {
        let c = CString::new(name).map_err(|_| "a NUL in the function's name".to_string())?;
        // Safety: a handle `open` answered, and a C string.
        let at = unsafe { GetProcAddress(library as *mut c_void, c.as_ptr()) };
        if at.is_null() {
            // Safety: none needed.
            Err(format!("no `{name}`: Windows error {}", unsafe {
                GetLastError()
            }))
        } else {
            Ok(at as i64)
        }
    }

    /// A word as the type of the place it is passed in.
    trait Placed: Copy {
        fn of(w: u64) -> Self;
    }
    impl Placed for i64 {
        fn of(w: u64) -> i64 {
            w as i64
        }
    }
    impl Placed for f64 {
        fn of(w: u64) -> f64 {
            f64::from_bits(w)
        }
    }

    /// The call, its first four arguments of types `A` to `D`: what came
    /// back in the integer register and in the float register.
    ///
    /// # Safety
    ///
    /// `function` must be a C function of the arguments `w` holds.
    unsafe fn placed<A: Placed, B: Placed, C: Placed, D: Placed>(
        function: i64,
        w: &[u64; WORDS],
        float: bool,
    ) -> (i64, u64) {
        type Ints<A, B, C, D> = extern "C" fn(
            A,
            B,
            C,
            D,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) -> i64;
        type Floats<A, B, C, D> = extern "C" fn(
            A,
            B,
            C,
            D,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) -> f64;
        let r = |k: usize| w[k] as i64;
        // Safety: the caller's.
        unsafe {
            if float {
                let f: Floats<A, B, C, D> = std::mem::transmute(function as *const c_void);
                let x = f(
                    A::of(w[0]),
                    B::of(w[1]),
                    C::of(w[2]),
                    D::of(w[3]),
                    r(4),
                    r(5),
                    r(6),
                    r(7),
                    r(8),
                    r(9),
                    r(10),
                    r(11),
                    r(12),
                    r(13),
                    r(14),
                    r(15),
                );
                (0, x.to_bits())
            } else {
                let f: Ints<A, B, C, D> = std::mem::transmute(function as *const c_void);
                let n = f(
                    A::of(w[0]),
                    B::of(w[1]),
                    C::of(w[2]),
                    D::of(w[3]),
                    r(4),
                    r(5),
                    r(6),
                    r(7),
                    r(8),
                    r(9),
                    r(10),
                    r(11),
                    r(12),
                    r(13),
                    r(14),
                    r(15),
                );
                (n, 0)
            }
        }
    }

    pub fn call(function: i64, ret: Ret, args: &[Arg]) -> Result<Answer, String> {
        if function == 0 {
            return Err("a call to a null function".to_string());
        }
        if args.len() > WORDS {
            return Err(format!("more than {WORDS} arguments"));
        }
        let mut w = [0u64; WORDS];
        let mut mask = 0u8;
        // The strings, kept until the call returns.
        let mut texts: Vec<CString> = Vec::new();
        for (k, a) in args.iter().enumerate() {
            let (word, float) = super::word(a, &mut texts)?;
            w[k] = word;
            if float && k < 4 {
                mask |= 1 << k;
            }
        }
        let float = ret.in_float();
        type I = i64;
        type F = f64;
        // Safety: none that can be checked; see `placed`.
        let (int, bits) = unsafe {
            match mask {
                0 => placed::<I, I, I, I>(function, &w, float),
                1 => placed::<F, I, I, I>(function, &w, float),
                2 => placed::<I, F, I, I>(function, &w, float),
                3 => placed::<F, F, I, I>(function, &w, float),
                4 => placed::<I, I, F, I>(function, &w, float),
                5 => placed::<F, I, F, I>(function, &w, float),
                6 => placed::<I, F, F, I>(function, &w, float),
                7 => placed::<F, F, F, I>(function, &w, float),
                8 => placed::<I, I, I, F>(function, &w, float),
                9 => placed::<F, I, I, F>(function, &w, float),
                10 => placed::<I, F, I, F>(function, &w, float),
                11 => placed::<F, F, I, F>(function, &w, float),
                12 => placed::<I, I, F, F>(function, &w, float),
                13 => placed::<F, I, F, F>(function, &w, float),
                14 => placed::<I, F, F, F>(function, &w, float),
                _ => placed::<F, F, F, F>(function, &w, float),
            }
        };
        let answered = super::answer(ret, int, bits);
        drop(texts);
        Ok(answered)
    }

    pub fn alloc(bytes: i64) -> i64 {
        // Safety: `calloc`, which answers null when it cannot.
        unsafe { calloc(1, bytes.max(1) as usize) as i64 }
    }

    pub fn free(at: i64) {
        // Safety: an address `alloc` or C answered, the caller says.
        unsafe { c_free(at as *mut c_void) }
    }
}

#[cfg(not(any(unix, all(windows, target_arch = "x86_64"))))]
mod sys {
    use super::{Answer, Arg, Ret};

    const NOT_YET: &str = "calling C is not supported on this platform yet";

    pub fn open(_path: &str) -> Result<i64, String> {
        Err(NOT_YET.to_string())
    }

    pub fn symbol(_library: i64, _name: &str) -> Result<i64, String> {
        Err(NOT_YET.to_string())
    }

    pub fn call(_function: i64, _ret: Ret, _args: &[Arg]) -> Result<Answer, String> {
        Err(NOT_YET.to_string())
    }

    pub fn alloc(_bytes: i64) -> i64 {
        0
    }

    pub fn free(_at: i64) {}
}

/// Open the library at `path` -- or of that name, wherever the system's
/// loader looks -- answering a handle to it, or why not. The empty path is
/// the library this code is in: the program itself, or the plugin it is.
pub fn open(path: &str) -> Result<i64, String> {
    if path.is_empty() && EXPORTS.get().is_some() {
        return Ok(OWN);
    }
    sys::open(path)
}

/// The address of `name` in `library`, or why there is none.
pub fn symbol(library: i64, name: &str) -> Result<i64, String> {
    if library == OWN {
        if let Some(&(_, at)) = EXPORTS
            .get()
            .and_then(|t| t.iter().find(|(n, _)| n == name))
        {
            return Ok(at);
        }
        // Not one of the library's exports: what else the loader knows of
        // the module this is.
        return sys::symbol(sys::open("")?, name);
    }
    sys::symbol(library, name)
}

/// The C functions of the library of Meadow's this runtime is linked into,
/// by name: what `open("")` answers is looked in first. A library linked
/// into the program that uses it -- iOS's, which is static -- is part of an
/// executable whose names the loader may not know, a release build having
/// stripped the names nothing calls by. The runtime is given them as the
/// library is first entered.
static EXPORTS: std::sync::OnceLock<Vec<(String, i64)>> = std::sync::OnceLock::new();

/// What `open("")` answers in a library that has its table: odd, so no
/// handle a loader gives.
const OWN: i64 = 1;

/// Keep `table`, the library's C functions by name, for `open("")`.
pub fn set_exports(table: Vec<(String, i64)>) {
    let _ = EXPORTS.set(table);
}

/// Call the C function at `function` with `args`, answering as `ret` says.
pub fn call(function: i64, ret: Ret, args: &[Arg]) -> Result<Answer, String> {
    sys::call(function, ret, args)
}

/// `bytes` bytes of C's memory, zeroed: 0 if there are none to be had.
pub fn alloc(bytes: i64) -> i64 {
    sys::alloc(bytes)
}

/// Give back what [`alloc`], or C, allocated.
pub fn free(at: i64) {
    sys::free(at)
}

/// The null address is nobody's to read or write: what an allocation that
/// failed answers, and what `alloc` answers where there is no C to ask. Said
/// as an error, where following it would end the program.
fn not_null(at: i64) -> Result<(), String> {
    if at == 0 {
        Err("the null address".to_string())
    } else {
        Ok(())
    }
}

/// Write `values` from `at` on, each as an integer of `width` bytes (1, 2, 4
/// or 8), or say the width is none of those.
///
/// # Safety
///
/// `at` must be the address of that many bytes the program may write.
pub unsafe fn write_ints(at: i64, width: i64, values: &[i64]) -> Result<(), String> {
    not_null(at)?;
    for (i, v) in values.iter().enumerate() {
        // Safety: the caller's.
        unsafe {
            match width {
                1 => (at as *mut u8).add(i).write_unaligned(*v as u8),
                2 => (at as *mut u16).add(i).write_unaligned(*v as u16),
                4 => (at as *mut u32).add(i).write_unaligned(*v as u32),
                8 => (at as *mut i64).add(i).write_unaligned(*v),
                _ => return Err(format!("an integer of {width} bytes")),
            }
        }
    }
    Ok(())
}

/// Read `count` integers of `width` bytes from `at`, signed when `signed`.
///
/// # Safety
///
/// `at` must be the address of that many bytes the program may read.
pub unsafe fn read_ints(at: i64, width: i64, signed: bool, count: i64) -> Result<Vec<i64>, String> {
    not_null(at)?;
    let mut out = Vec::with_capacity(count.max(0) as usize);
    for i in 0..count.max(0) as usize {
        // Safety: the caller's.
        out.push(unsafe {
            match (width, signed) {
                (1, false) => i64::from((at as *const u8).add(i).read_unaligned()),
                (1, true) => i64::from((at as *const i8).add(i).read_unaligned()),
                (2, false) => i64::from((at as *const u16).add(i).read_unaligned()),
                (2, true) => i64::from((at as *const i16).add(i).read_unaligned()),
                (4, false) => i64::from((at as *const u32).add(i).read_unaligned()),
                (4, true) => i64::from((at as *const i32).add(i).read_unaligned()),
                (8, _) => (at as *const i64).add(i).read_unaligned(),
                _ => return Err(format!("an integer of {width} bytes")),
            }
        });
    }
    Ok(out)
}

/// Write `values` from `at` on, each as a float of `width` bytes (4 or 8).
///
/// # Safety
///
/// `at` must be the address of that many bytes the program may write.
pub unsafe fn write_floats(at: i64, width: i64, values: &[f64]) -> Result<(), String> {
    not_null(at)?;
    for (i, v) in values.iter().enumerate() {
        // Safety: the caller's.
        unsafe {
            match width {
                4 => (at as *mut f32).add(i).write_unaligned(*v as f32),
                8 => (at as *mut f64).add(i).write_unaligned(*v),
                _ => return Err(format!("a float of {width} bytes")),
            }
        }
    }
    Ok(())
}

/// Read `count` floats of `width` bytes from `at`.
///
/// # Safety
///
/// `at` must be the address of that many bytes the program may read.
pub unsafe fn read_floats(at: i64, width: i64, count: i64) -> Result<Vec<f64>, String> {
    not_null(at)?;
    let mut out = Vec::with_capacity(count.max(0) as usize);
    for i in 0..count.max(0) as usize {
        // Safety: the caller's.
        out.push(unsafe {
            match width {
                4 => f64::from((at as *const f32).add(i).read_unaligned()),
                8 => (at as *const f64).add(i).read_unaligned(),
                _ => return Err(format!("a float of {width} bytes")),
            }
        });
    }
    Ok(out)
}

/// The `count` bytes at `at`, as text: what is not UTF-8 is replaced.
///
/// # Safety
///
/// `at` must be the address of that many bytes the program may read.
pub unsafe fn read_text(at: i64, count: i64) -> String {
    // Nothing is at the null address: no text, rather than a crash.
    if at == 0 {
        return String::new();
    }
    // Safety: the caller's.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, count.max(0) as usize) };
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The C library is every process's own: opened by no name.
    fn libc_fn(name: &str) -> i64 {
        // Safety: asking the running program for one of its own symbols.
        let at = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                std::ffi::CString::new(name).unwrap().as_ptr(),
            )
        };
        assert!(!at.is_null(), "{name}");
        at as i64
    }

    #[test]
    fn integers_strings_and_floats_cross() {
        let strlen = libc_fn("strlen");
        assert_eq!(
            call(strlen, Ret::Int, &[Arg::Str("héllo".into())])
                .unwrap()
                .0,
            6
        );
        let pow = libc_fn("pow");
        assert_eq!(
            call(pow, Ret::Float, &[Arg::Float(2.0), Arg::Float(10.0)])
                .unwrap()
                .1,
            1024.0
        );
        let ldexp = libc_fn("ldexp");
        assert_eq!(
            call(ldexp, Ret::Float, &[Arg::Float(1.5), Arg::Int(4)])
                .unwrap()
                .1,
            24.0
        );
        let getenv = libc_fn("getenv");
        let path = call(getenv, Ret::Str, &[Arg::Str("PATH".into())])
            .unwrap()
            .2;
        assert!(path.contains('/'), "{path}");
        let none = call(
            getenv,
            Ret::Str,
            &[Arg::Str("MEADOW_NO_SUCH_VARIABLE".into())],
        )
        .unwrap()
        .2;
        assert_eq!(none, "", "a null string is the empty one");
    }

    #[test]
    fn memory_is_written_and_read_back() {
        let at = alloc(64);
        assert_ne!(at, 0);
        // Safety: 64 bytes, ours.
        unsafe {
            write_floats(at, 4, &[1.5, -2.25, 3.0]).unwrap();
            assert_eq!(read_floats(at, 4, 3).unwrap(), vec![1.5, -2.25, 3.0]);
            write_ints(at, 1, &[104, 105, 255]).unwrap();
            assert_eq!(read_ints(at, 1, false, 3).unwrap(), vec![104, 105, 255]);
            assert_eq!(read_ints(at, 1, true, 3).unwrap(), vec![104, 105, -1]);
            assert_eq!(read_text(at, 2), "hi");
            assert!(write_ints(at, 3, &[1]).is_err());
        }
        free(at);
    }

    #[test]
    fn what_cannot_be_called_is_refused() {
        assert!(open("/no/such/library.dylib").is_err());
        assert!(call(0, Ret::Int, &[]).is_err());
        let floats: Vec<Arg> = (0..9).map(|_| Arg::Float(0.0)).collect();
        assert!(call(libc_fn("pow"), Ret::Float, &floats).is_err());
    }
}
