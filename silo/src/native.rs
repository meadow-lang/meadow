//! **What an unhandled effect operation means**: the real world's answer.
//!
//! What each operation does is `meadow_rt::world`'s, the same on every
//! runtime; this is Silo's side of it -- how its values are read, and how an
//! answer is made of blocks ([`World`] for [`Here`]). `Test.fail` is a failure
//! rather than a value, and is here.

use crate::heap::{self, Word};
use crate::show;
use crate::value::{self, Val, val};
use meadow_rt::desc;
use meadow_rt::roles::{Role, TUPLE};
use meadow_rt::world::{Imm, Job, World};

fn fail<T>(msg: impl AsRef<str>) -> T {
    crate::fail(msg.as_ref())
}

/// A result, built bottom-up. Nothing moves, so it is built as it is made.
type Build = meadow_rt::world::Build<Val>;

impl Imm for Val {
    fn unit() -> Self {
        Val::Unit
    }
    fn int(n: i64) -> Self {
        Val::Int(n)
    }
    fn float(x: f64) -> Self {
        Val::Float(x)
    }
    fn boolean(b: bool) -> Self {
        Val::Bool(b)
    }
}

fn make(b: Build) -> Val {
    match b {
        Build::At(v) => v,
        Build::Str(s) => Val::Ref(heap::string(s.as_bytes())),
        Build::Data(role, xs) => {
            let fields: Vec<Val> = xs.into_iter().map(make).collect();
            Val::Ref(value::data(show::role(role), &fields))
        }
        Build::Tuple(xs) => {
            let fields: Vec<Val> = xs.into_iter().map(make).collect();
            Val::Ref(value::data(TUPLE, &fields))
        }
        Build::Bytes(b) => {
            let words: Vec<Word> = b.iter().map(|x| Word::from(*x)).collect();
            Val::Ref(crate::prims::new_array(
                heap::ARRAY,
                &words,
                desc::word(meadow_rt::num::Width::U8),
            ))
        }
        Build::Vector(xs) => {
            let items: Vec<Val> = xs.into_iter().map(make).collect();
            Val::Ref(vector(items))
        }
        // Only an embedding host answers with the program's own constructors,
        // and Silo has none.
        Build::Ctor(name, _) => fail(format!("a host's constructor `{name}`, with no host")),
    }
}

/// `vWidth` in `Std.Collections.Vector`.
const VECTOR_WIDTH: usize = 32;

/// `items`, owned, laid out as `Vector.fromArray` would -- which has to stay
/// `vBuildTree` in `Std.Collections.Vector`, since that module takes it apart.
fn vector(items: Vec<Val>) -> Word {
    let arr = |xs: &[Val]| {
        let d = xs.first().map_or(desc::REF, |v| v.bits().1);
        let words: Vec<Word> = xs.iter().map(|v| v.word()).collect();
        Val::Ref(crate::prims::new_array(heap::ARRAY, &words, d))
    };
    // A program that declares no vector gets a plain array (`docs/CUT.md`).
    if show::role(Role::VectorSingle).is_empty() {
        return arr(&items).word();
    }
    if items.is_empty() {
        return value::data(show::role(Role::VectorEmpty), &[]);
    }
    if items.len() <= VECTOR_WIDTH {
        let a = arr(&items);
        return value::data(show::role(Role::VectorSingle), &[a]);
    }
    let n = items.len();
    let mut shift = 5i64;
    let mut count = n.div_ceil(VECTOR_WIDTH);
    while count > VECTOR_WIDTH {
        count = count.div_ceil(VECTOR_WIDTH);
        shift += 5;
    }
    let mut nodes: Vec<Val> = items
        .chunks(VECTOR_WIDTH)
        .map(|chunk| Val::Ref(value::data(show::role(Role::VNodeLeaf), &[arr(chunk)])))
        .collect();
    loop {
        let group = |kids: &[Val]| {
            let none = Val::Ref(value::data(show::role(Role::None), &[]));
            Val::Ref(value::data(
                show::role(Role::VNodeBranch),
                &[none, arr(kids)],
            ))
        };
        if nodes.len() <= VECTOR_WIDTH {
            let root = group(&nodes);
            let empty = || arr(&[]);
            return value::data(
                show::role(Role::VectorFull),
                &[
                    Val::Int(n as i64),
                    Val::Int(shift),
                    empty(),
                    empty(),
                    root,
                    empty(),
                    empty(),
                ],
            );
        }
        nodes = nodes.chunks(VECTOR_WIDTH).map(group).collect();
    }
}

fn shown(v: Val) -> String {
    let (w, d) = v.bits();
    show::show(w, d)
}

/// The runtime, as `meadow_rt::world` asks of it.
struct Here;

impl World for Here {
    type Value = Val;

    /// What the engine has noted, which native code never does: see
    /// `crate::coverage`.
    fn covered(&mut self, reset: bool) -> i64 {
        crate::coverage::covered(reset)
    }

    fn int_of(&self, v: Val) -> Option<i64> {
        match v {
            Val::Int(n) => Some(n),
            _ => None,
        }
    }

    fn float_of(&self, v: Val) -> Option<f64> {
        match v {
            Val::Float(x) => Some(x),
            _ => None,
        }
    }

    fn bool_of(&self, v: Val) -> Option<bool> {
        match v {
            Val::Bool(b) => Some(b),
            _ => None,
        }
    }

    fn text(&self, v: Val, what: &str) -> Result<String, String> {
        match value::text(v) {
            Some(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
            None => Err(format!("{what}: expected a String, got {}", shown(v))),
        }
    }

    fn bytes(&self, v: Val, what: &str) -> Result<Vec<u8>, String> {
        Ok(crate::prims::byte_array(v, what))
    }

    fn tuple(&self, v: Val, n: usize, what: &str) -> Result<Vec<Val>, String> {
        match v {
            Val::Ref(w)
                if heap::is_block(w)
                    && heap::kind(w) == heap::DATA
                    && heap::len(w) == n
                    && show::ctor_name(heap::meta(w) as usize) == TUPLE =>
            {
                Ok((0..n)
                    .map(|i| val(heap::field(w, i), heap::field_desc(w, i)))
                    .collect())
            }
            other => Err(format!(
                "{what}: expected a tuple of {n}, got {}",
                shown(other)
            )),
        }
    }

    fn seq(&self, v: Val, what: &str) -> Result<Vec<Val>, String> {
        match v {
            Val::Ref(w) => match show::vector_elems(w).or_else(|| show::array_elems(w)) {
                Some(xs) => Ok(xs.into_iter().map(|(x, d)| val(x, d)).collect()),
                None => Err(format!("{what}: expected a Vector, got {}", shown(v))),
            },
            other => Err(format!("{what}: expected a Vector, got {}", shown(other))),
        }
    }

    fn maybe(&self, v: Val, what: &str) -> Result<Option<Val>, String> {
        if let Val::Ref(w) = v {
            if heap::is_immediate(w)
                && show::role_of(&show::ctor_name(heap::payload(w) as usize)) == Some(Role::None)
            {
                return Ok(None);
            }
            if heap::is_block(w)
                && heap::kind(w) == heap::DATA
                && show::role_of(&show::ctor_name(heap::meta(w) as usize)) == Some(Role::Some)
            {
                return Ok(Some(val(heap::field(w, 0), heap::field_desc(w, 0))));
            }
        }
        Err(format!("{what}: expected a Maybe, got {}", shown(v)))
    }

    fn shown(&self, v: Val) -> String {
        shown(v)
    }

    /// Whatever it is, as it displays: a string as its text.
    fn printed(&self, v: Val, _what: &str) -> Result<String, String> {
        Ok(show::displayed(v.word(), v.bits().1))
    }

    /// A native program is its own process, and what it was given is its
    /// own -- unless something runs it, as Silo's interpreter does, and says
    /// what it was given.
    fn argv(&self) -> Vec<String> {
        meadow_rt::args::get()
    }

    fn ffi_failure(&mut self) -> String {
        crate::sched::take_failure().unwrap_or_default()
    }

    fn exit(&mut self, code: i32) -> ! {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        // What a run that ends here was asked to report, as one that returns
        // from `main` reports it: its profile, and what `MEADOW_SILO_PRIMS`
        // and `MEADOW_SILO_CYCLES` count.
        crate::profile::finish();
        crate::prims::report();
        crate::cycles::report();
        std::process::exit(code)
    }

    /// Made by an I/O thread while this green thread waits: see
    /// `sched::blocking`.
    fn blocking(&mut self, job: Job<Val>) -> Build {
        crate::sched::blocking(job)
    }
}

/// An effect operation no handler answers: the world's.
///
/// # Safety
///
/// `effect` and `op` must be NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_native(
    effect: *const std::ffi::c_char,
    op: *const std::ffi::c_char,
    arg: Word,
    d: i64,
) -> Word {
    // Safety: the caller's.
    let (effect, op) = unsafe {
        (
            std::ffi::CStr::from_ptr(effect).to_string_lossy(),
            std::ffi::CStr::from_ptr(op).to_string_lossy(),
        )
    };
    let arg = val(arg, d);
    if meadow_rt::spelling(&effect) == "Test" && op == "fail" {
        return fail(show::displayed(arg.word(), d));
    }
    match meadow_rt::world::perform(&mut Here, &effect, &op, arg) {
        Ok(Some(b)) => make(b).word(),
        Ok(None) => fail(format!(
            "unhandled effect {}.{op}",
            meadow_rt::spelling(&effect)
        )),
        Err(msg) => fail(msg),
    }
}
