//! Running a program compiled ahead of time.
//!
//! A native executable is its machine code and its image, in an object file
//! written by [`crate::codegen::object`], linked with this runtime and a `main`
//! that calls [`meadow_silo_main`]. The image is the program's tables and its
//! bytecode, which the instructions native code hands back to the
//! interpreter still run from; the machine code is every block that has some.
//!
//! A library is the same object and runtime, linked with a C function for
//! each definition it exports in place of `main`
//! ([`crate::codegen::object::library_c`]), each of which calls
//! [`meadow_glade_export`].

use crate::abi::NativeFn;
use crate::value::Value;
use meadow_bytecode::{Op, Pc, Program};
use meadow_rt::num::Width;
use std::cell::RefCell;
use std::ffi::c_char;
use std::sync::OnceLock;

/// Defined only by a runtime library built from the sources the code generator
/// was: see `build.rs`, and [`crate::codegen::object::runtime_symbol`].
#[unsafe(export_name = concat!("meadow_glade_", env!("MEADOW_GLADE_FINGERPRINT")))]
pub static FINGERPRINT: u8 = 0;

/// Run the program in `data`, with the native code starting at `code`, and
/// say how it went as a process exit status: its answer printed, unless it
/// is `()`, and 0; or its failure, and 1.
///
/// # Safety
///
/// `code` and `data` must be the two symbols of one object file from
/// [`crate::codegen::object::write`], for the architecture this is running
/// on.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_silo_main(
    code: *const u8,
    data: *const u8,
    _argc: i32,
    _argv: *const *const c_char,
) -> i32 {
    // Safety: the caller's.
    let (program, blocks, stubs) = match unsafe { read(data) } {
        Ok(read) => read,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    // Safety: the caller's.
    let native = unsafe { native(&program, code, blocks, stubs) };
    let Some(entry) = program.entry else {
        eprintln!("error: the program has no entry point");
        return 1;
    };
    let outcome = crate::sched::run_native(
        &program,
        Some(&native),
        entry,
        u64::MAX,
        crate::sched::workers(),
    );
    if crate::abi::traps::on() {
        traps(&program);
    }
    match outcome.result {
        Ok(answer) => {
            if answer != "()" {
                println!("{answer}");
            }
            0
        }
        Err(e) => {
            eprintln!("{}", e.msg);
            1
        }
    }
}

/// Block and method entries: (entry pc, offset into the code), per entry.
type Entries = Vec<(u32, u32)>;

/// The program in `data`, and its blocks' and method entries' native code as
/// offsets into the code.
///
/// # Safety
///
/// `data` must be the data symbol of an object file from
/// [`crate::codegen::object::write`].
unsafe fn read(data: *const u8) -> Result<(Program, Entries, Entries), String> {
    // Safety: the layout `object::data` writes, which the caller vouches for.
    let (image, blocks, stubs) = unsafe {
        let word = |at: usize| (data.add(at) as *const u64).read_unaligned() as usize;
        let (len, count, table) = (word(0), word(8), word(16));
        let (stub_count, stub_table) = (word(24), word(32));
        let image = std::slice::from_raw_parts(data.add(40), len);
        let entries = |table: usize, count: usize| -> Vec<(u32, u32)> {
            (0..count)
                .map(|i| {
                    let at = data.add(table + 8 * i);
                    (
                        (at as *const u32).read_unaligned(),
                        (at.add(4) as *const u32).read_unaligned(),
                    )
                })
                .collect()
        };
        (
            image,
            entries(table, count),
            entries(stub_table, stub_count),
        )
    };
    let program = meadow_bytecode::image::decode(image)
        .map_err(|e| format!("the program's image is damaged: {e}"))?;
    Ok((program, blocks, stubs))
}

/// `program`'s native code, at the offsets `blocks` and `stubs` give from
/// `code`.
///
/// # Safety
///
/// `code` must be the code symbol of the object file `program` was read from,
/// for the architecture this is running on.
unsafe fn native<'p>(
    program: &'p Program,
    code: *const u8,
    blocks: Entries,
    stubs: Entries,
) -> crate::jit::Native<'p> {
    crate::jit::Native::ahead_of_time(
        program,
        blocks.into_iter().map(|(pc, offset)| {
            // Safety: an offset the compiler recorded for the function it
            // emitted there, in code the linker made executable.
            let f: NativeFn = unsafe { std::mem::transmute(code.add(offset as usize)) };
            (pc, f)
        }),
        stubs.into_iter().map(|(pc, offset)| {
            // Safety: likewise, for the method entry.
            (pc, unsafe { code.add(offset as usize) } as *const u8)
        }),
    )
}

/// What `MEADOW_TRAPS` reports when the program ends: the instructions native
/// code handed to the interpreter, by pc and by what they are.
fn traps(program: &Program) {
    {
        let all = crate::abi::traps::report();
        let total: u64 = all.iter().map(|(_, c)| c).sum();
        eprintln!("traps: {total} instructions handed to the interpreter, by pc:");
        for ((pc, op), c) in all.iter().take(25) {
            eprintln!(
                "  {c:>12}  {:5.1}%  pc {pc:<6} {op:?}",
                100.0 * *c as f64 / total as f64
            );
        }
        // And by what the instruction is, over every pc: which primitive,
        // and for a field which one -- what there is to make native next.
        let mut kinds: std::collections::HashMap<String, u64> = Default::default();
        for ((pc, op), c) in &all {
            let i = program.code.get(*pc as usize);
            let kind = match (op, i) {
                (Some(Op::Prim | Op::Prim1 | Op::Prim2), Some(i)) => {
                    match program.prims.get(i.imm as usize) {
                        Some(p) => format!("{p:?}"),
                        None => format!("{op:?}"),
                    }
                }
                (Some(Op::Field), Some(i)) => format!("Field {}", i.imm),
                (Some(op), _) => format!("{op:?}"),
                (None, _) => "?".to_string(),
            };
            *kinds.entry(kind).or_default() += c;
        }
        let mut kinds: Vec<_> = kinds.into_iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        eprintln!("traps, by instruction:");
        for (kind, c) in kinds.iter().take(25) {
            eprintln!(
                "  {c:>12}  {:5.1}%  {kind}",
                100.0 * *c as f64 / total as f64
            );
        }
    }
}

/// What a C argument or answer is, as [`meadow_glade_export`] is told it: C
/// hands every one over as a 64-bit word, and this says what the word holds.
pub mod kind {
    pub const UNIT: u8 = 0;
    /// An `int64_t`: an `Int`.
    pub const INT: u8 = 1;
    /// A `double`'s bits: a `Float`.
    pub const FLOAT: u8 = 2;
    /// A `float`'s bits, in the low half: a `Float32`.
    pub const FLOAT32: u8 = 3;
    pub const BOOL: u8 = 4;
    /// A code point.
    pub const CHAR: u8 = 5;
    /// A sized integer, extended to 64 bits: `WORD` plus the width's place in
    /// [`meadow_rt::num::Width::ALL`].
    pub const WORD: u8 = 8;

    /// The kind of a value whose word is as descriptor `d` says: `None` for
    /// one C does not pass.
    pub fn of_bits(d: meadow_rt::desc::Desc) -> Option<u8> {
        use meadow_rt::desc;
        if d == desc::FLOAT32 {
            Some(FLOAT32)
        } else if d == desc::BOOL {
            Some(BOOL)
        } else if d == desc::CHAR {
            Some(CHAR)
        } else if d == desc::UNIT {
            Some(UNIT)
        } else {
            meadow_rt::num::Width::ALL
                .iter()
                .position(|w| d == desc::word(*w))
                .map(|at| WORD + at as u8)
        }
    }
}

/// The library's program and native code, read the first time C calls into
/// it, and kept: the image is in the library, which is loaded for as long as
/// anything calls it.
struct Library {
    program: &'static Program,
    native: &'static crate::jit::Native<'static>,
}

static LIBRARY: OnceLock<Result<Library, String>> = OnceLock::new();

thread_local! {
    /// Why the last call C made into the library on this thread failed, until
    /// `Std.Ffi.failure` asks.
    static FAILURE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Why the last call C made into this library on this OS thread failed, once.
pub fn take_failure() -> Option<String> {
    FAILURE.with(|f| f.borrow_mut().take())
}

/// The value C's word `w` is, as `kind` says.
fn value_of(kind: u8, w: u64) -> Result<Value, String> {
    Ok(match kind {
        kind::UNIT => Value::Unit,
        kind::INT => Value::Int(w as i64),
        kind::FLOAT => Value::Float(f64::from_bits(w)),
        kind::FLOAT32 => Value::Float32(f32::from_bits(w as u32)),
        kind::BOOL => Value::Bool(w & 0xff != 0),
        kind::CHAR => Value::Char(
            char::from_u32(w as u32).ok_or_else(|| format!("{w:#x} is not a character"))?,
        ),
        k if k >= kind::WORD && ((k - kind::WORD) as usize) < Width::ALL.len() => {
            let width = Width::ALL[(k - kind::WORD) as usize];
            let x = if width.signed() {
                w as i64 as i128
            } else {
                w as i128
            };
            Value::Word(width, width.wrap(x))
        }
        k => return Err(format!("an argument of kind {k}, which C does not pass")),
    })
}

/// The word C is answered for `v`: its bits, which C's own type is cut from.
fn word_of(v: Value) -> Result<u64, String> {
    Ok(match v {
        Value::Unit => 0,
        Value::Bool(b) => b as u64,
        Value::Int(i) => i as u64,
        Value::Word(_, b) => b,
        Value::Float(f) => f.to_bits(),
        Value::Float32(f) => f.to_bits() as u64,
        Value::Char(c) => c as u64,
        Value::Str(_) | Value::Obj(_) => {
            return Err("the call answered what C cannot take".to_string());
        }
    })
}

/// Call definition `def` of the library whose object file's symbols are
/// `code` and `data`, with the `n` words at `args`, each of the kind at the
/// same place in `kinds` ([`kind`]); write its answer to `out` as a word, and
/// answer 0. Or, if the call fails, write 0, say why on standard error and to
/// `Std.Ffi.failure`, and answer 1: C goes on.
///
/// Each call is a run of its own: its heap is made for it and goes with it,
/// and so does every top-level value it makes.
///
/// # Safety
///
/// `code` and `data` must be the two symbols of one object file from
/// [`crate::codegen::object::write`], for the architecture this is running on;
/// `args` and `kinds` must hold `n` each; and `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_glade_export(
    code: *const u8,
    data: *const u8,
    def: u32,
    args: *const u64,
    kinds: *const u8,
    n: u32,
    out: *mut u64,
) -> i32 {
    let library = LIBRARY.get_or_init(|| {
        // Safety: the caller's.
        let (program, blocks, stubs) = unsafe { read(data) }?;
        let program: &'static Program = Box::leak(Box::new(program));
        // Safety: the caller's.
        let native = Box::leak(Box::new(unsafe { native(program, code, blocks, stubs) }));
        Ok(Library { program, native })
    });
    let called = library.as_ref().map_err(|e| e.clone()).and_then(|l| {
        let entry: Pc = *l
            .program
            .entries
            .get(def as usize)
            .ok_or_else(|| format!("the library has no definition {def}"))?;
        let given = (0..n as usize)
            .map(|i| {
                // Safety: the caller's.
                let (k, w) = unsafe { (*kinds.add(i), *args.add(i)) };
                value_of(k, w)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let answer =
            crate::sched::run_export(l.program, Some(l.native), entry, given).map_err(|e| e.msg)?;
        word_of(answer)
    });
    match called {
        Ok(w) => {
            // Safety: the caller's.
            unsafe { *out = w };
            0
        }
        Err(why) => {
            // Safety: the caller's.
            unsafe { *out = 0 };
            use std::io::Write;
            let _ = std::io::stdout().flush();
            eprintln!("{why}");
            FAILURE.with(|f| *f.borrow_mut() = Some(why));
            1
        }
    }
}
