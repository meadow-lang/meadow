//! A value as text, exactly as the CEK machine renders it -- the differential
//! tests compare the machines' output character for character -- and the
//! program's tables it reads names out of.

use crate::heap::{self, Word};
use meadow_rt::desc;
use meadow_rt::roles::Role;

/// The program's tables the emitted module defines: constructor names and
/// named fields by tag, and interned names -- with the order records keep
/// their labels in -- by the small integers `Sym` literals load.
pub struct Names {
    pub ctors: Vec<Option<String>>,
    /// The constructor playing each role, by [`Role::index`].
    pub roles: Vec<Option<String>>,
    pub fields: Vec<Option<Vec<String>>>,
    pub syms: Vec<String>,
    pub ranks: Vec<i64>,
}

unsafe extern "C" {
    static meadow_ctor_names: [*const std::ffi::c_char; 0];
    static meadow_ctor_fields: [*const std::ffi::c_char; 0];
    static meadow_ctor_count: i64;
    static meadow_role_names: [*const std::ffi::c_char; 0];
    static meadow_sym_names: [*const std::ffi::c_char; 0];
    static meadow_sym_ranks: [i64; 0];
    static meadow_sym_count: i64;
}

/// Tables given by whatever runs the program when there is no emitted module
/// to read them from: an interpreter (`silo/vm`), before the program starts,
/// and again when it loads a new build of the program. Each is kept for as
/// long as the program runs -- what read one may still be reading it -- and
/// each extends the one before: what the old numbered, it numbers alike.
static INSTALLED: std::sync::atomic::AtomicPtr<Names> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Give the program's tables to the runtime, before anything asks for them.
pub fn install(names: Names) {
    INSTALLED.store(
        Box::into_raw(Box::new(names)),
        std::sync::atomic::Ordering::Release,
    );
}

/// Replace the tables, as loading a new build of the running program does.
/// `make` is given the labels the runtime numbered after the program's own
/// (see [`sym_named`]), and answers tables whose symbols number those as they
/// were -- after which they are the program's, and not numbered again. Until
/// it answers, nothing numbers another.
pub fn reinstall<E>(make: impl FnOnce(&[String]) -> Result<Names, E>) -> Result<(), E> {
    let mut extra = EXTRA.lock().unwrap_or_else(|p| p.into_inner());
    let names = make(&extra)?;
    install(names);
    extra.clear();
    Ok(())
}

/// The program's tables: those installed, or the emitted module's, read once.
pub fn names() -> &'static Names {
    let installed = INSTALLED.load(std::sync::atomic::Ordering::Acquire);
    if !installed.is_null() {
        // Safety: installed tables are never freed.
        return unsafe { &*installed };
    }
    static NAMES: std::sync::OnceLock<Names> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let read = |p: *const std::ffi::c_char| {
            (!p.is_null()).then(|| {
                // Safety: a NUL-terminated string the emitted module defines.
                unsafe { std::ffi::CStr::from_ptr(p) }
                    .to_string_lossy()
                    .into_owned()
            })
        };
        // Safety: arrays of these lengths, which the emitted module defines.
        unsafe {
            let n = meadow_ctor_count as usize;
            let ctors = (0..n)
                .map(|i| read(*meadow_ctor_names.as_ptr().add(i)))
                .collect();
            let fields = (0..n)
                .map(|i| {
                    read(*meadow_ctor_fields.as_ptr().add(i))
                        .map(|s| s.split(',').map(str::to_string).collect())
                })
                .collect();
            let roles = (0..Role::ALL.len())
                .map(|i| read(*meadow_role_names.as_ptr().add(i)))
                .collect();
            let m = meadow_sym_count as usize;
            let syms = (0..m)
                .map(|i| read(*meadow_sym_names.as_ptr().add(i)).unwrap_or_default())
                .collect();
            let ranks = (0..m).map(|i| *meadow_sym_ranks.as_ptr().add(i)).collect();
            Names {
                ctors,
                roles,
                fields,
                syms,
                ranks,
            }
        }
    })
}

/// Labels the runtime makes records with that the program never names --
/// numbered after the program's own.
static EXTRA: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The name of symbol `i`.
pub fn sym_name(i: usize) -> String {
    // The tables read with the labels held, which a reload moves into them.
    let extra = EXTRA.lock().unwrap_or_else(|p| p.into_inner());
    let n = names();
    match n.syms.get(i) {
        Some(s) => s.clone(),
        None => extra.get(i - n.syms.len()).cloned().unwrap_or_default(),
    }
}

/// The symbol called `name`.
pub fn sym_named(name: &str) -> usize {
    // As in `sym_name`.
    let mut extra = EXTRA.lock().unwrap_or_else(|p| p.into_inner());
    let n = names();
    if let Some(i) = n.syms.iter().position(|s| s == name) {
        return i;
    }
    let at = match extra.iter().position(|s| s == name) {
        Some(at) => at,
        None => {
            extra.push(name.to_string());
            extra.len() - 1
        }
    };
    n.syms.len() + at
}

/// Where symbol `i` goes among a record's labels.
pub fn sym_rank(i: usize) -> i64 {
    names().ranks.get(i).copied().unwrap_or(i as i64)
}

pub fn ctor_name(tag: usize) -> String {
    names()
        .ctors
        .get(tag)
        .cloned()
        .flatten()
        .unwrap_or_default()
}

/// The named fields of the `record` constructor `tag`, if it is one.
pub fn ctor_fields(tag: usize) -> Option<Vec<String>> {
    names().fields.get(tag).cloned().flatten()
}

/// Where the `record` constructor `tag` keeps the field named by symbol
/// `label`, if it has one.
pub fn ctor_field_index(tag: usize, label: usize) -> Option<usize> {
    let n = names();
    let name = n.syms.get(label)?;
    n.fields.get(tag)?.as_ref()?.iter().position(|f| f == name)
}

/// The constructor playing `role` -- empty, which no constructor is called,
/// where the program declares none. See `meadow_rt::roles`.
pub fn role(role: Role) -> &'static str {
    names().roles[role.index()].as_deref().unwrap_or("")
}

/// The role the constructor called `name` plays, if any.
pub fn role_of(name: &str) -> Option<Role> {
    if name.is_empty() {
        return None;
    }
    Role::ALL.into_iter().find(|r| role(*r) == name)
}

fn is_vector_ctor(name: &str) -> bool {
    matches!(
        role_of(name),
        Some(Role::VectorEmpty | Role::VectorSingle | Role::VectorFull)
    )
}

/// The constructor `v` was built with, if it is data.
fn ctor_of(v: Word) -> Option<String> {
    if heap::is_immediate(v) {
        return Some(ctor_name(heap::payload(v) as usize));
    }
    (heap::is_block(v) && heap::kind(v) == heap::DATA).then(|| ctor_name(heap::meta(v) as usize))
}

fn fields_of(v: Word) -> Vec<(Word, i64)> {
    if !heap::is_block(v) {
        return Vec::new();
    }
    (0..heap::len(v))
        .map(|i| (heap::field(v, i), heap::field_desc(v, i)))
        .collect()
}

/// The elements of a `Std.Collections.Vector`, if `v` is one.
pub fn vector_elems(v: Word) -> Option<Vec<(Word, i64)>> {
    // Asked of everything compared, hashed or shown, and nearly always of
    // what is not a vector: which constructors are a vector's is worked out
    // once, so that saying no costs no name.
    static VECTORS: std::sync::OnceLock<Vec<bool>> = std::sync::OnceLock::new();
    let tag = if heap::is_immediate(v) {
        heap::payload(v) as usize
    } else if heap::is_block(v) && heap::kind(v) == heap::DATA {
        heap::meta(v) as usize
    } else {
        return None;
    };
    let vectors = VECTORS.get_or_init(|| {
        names()
            .ctors
            .iter()
            .map(|c| c.as_deref().is_some_and(is_vector_ctor))
            .collect()
    });
    if !vectors.get(tag).copied().unwrap_or(false) {
        return None;
    }
    let name = ctor_of(v)?;
    if !is_vector_ctor(&name) {
        return None;
    }
    let f = fields_of(v);
    match (role_of(&name), f.len()) {
        (Some(Role::VectorEmpty), 0) => Some(Vec::new()),
        (Some(Role::VectorSingle), 1) => array_elems(f[0].0),
        (Some(Role::VectorFull), 7) => {
            let mut out = Vec::new();
            for i in [2usize, 3] {
                out.extend(array_elems(f[i].0)?);
            }
            vector_node(f[4].0, &mut out)?;
            for i in [5usize, 6] {
                out.extend(array_elems(f[i].0)?);
            }
            Some(out)
        }
        _ => None,
    }
}

fn vector_node(v: Word, out: &mut Vec<(Word, i64)>) -> Option<()> {
    let f = fields_of(v);
    match (role_of(&ctor_of(v)?), f.len()) {
        (Some(Role::VNodeLeaf), 1) => {
            out.extend(array_elems(f[0].0)?);
            Some(())
        }
        (Some(Role::VNodeBranch), 2) => {
            for (kid, _) in array_elems(f[1].0)? {
                vector_node(kid, out)?;
            }
            Some(())
        }
        _ => None,
    }
}

pub fn array_elems(v: Word) -> Option<Vec<(Word, i64)>> {
    (heap::is_block(v) && heap::kind(v) == heap::ARRAY).then(|| fields_of(v))
}

/// `v`, described by `d`, as `show` renders it (`meadow_rt::look`).
pub fn show(v: Word, d: i64) -> String {
    meadow_rt::look::show(&Shown, (v, d))
}

/// What `print` writes: a string as itself, anything else as shown.
pub fn displayed(v: Word, d: i64) -> String {
    meadow_rt::look::displayed(&Shown, (v, d))
}

/// A value and its descriptor, for the printer every runtime shares.
struct Shown;

impl meadow_rt::look::Look for Shown {
    type Value = (Word, i64);

    fn shape(&self, (v, d): (Word, i64)) -> meadow_rt::look::Shape<(Word, i64)> {
        use meadow_rt::look::Shape;
        match d {
            desc::INT => Shape::Int(v as i64),
            desc::FLOAT => Shape::Float(f64::from_bits(v)),
            desc::FLOAT32 => Shape::Float32(f32::from_bits(v as u32)),
            desc::BOOL => Shape::Bool(v != 0),
            desc::UNIT => Shape::Unit,
            desc::CHAR => Shape::Char(char::from_u32(v as u32)),
            desc::STR => Shape::Sym(sym_name(v as usize)),
            d if (desc::WORD..desc::WORD + 7).contains(&d) => {
                Shape::Word(meadow_rt::num::Width::ALL[(d - desc::WORD) as usize], v)
            }
            _ if v == 0 => Shape::Continuation,
            _ if heap::is_immediate(v) || heap::kind(v) == heap::DATA => {
                let tag = u64::from(heap::meta_of(v));
                let name = ctor_of(v).filter(|n| !n.is_empty());
                Shape::Data(name, tag, fields_of(v))
            }
            _ => match heap::kind(v) {
                heap::STRING => Shape::Str(String::from_utf8_lossy(&heap::bytes(v)).into_owned()),
                heap::BIGINT => Shape::BigInt(crate::value::bigint(v).expect("a BigInt's block")),
                heap::ARRAY => Shape::Array(fields_of(v)),
                heap::MUT_ARRAY => Shape::MutArray(fields_of(v)),
                heap::RECORD => {
                    let f = fields_of(v);
                    Shape::Record(
                        (0..f.len() / 2)
                            .map(|j| (sym_name(f[2 * j].0 as usize), f[2 * j + 1]))
                            .collect(),
                    )
                }
                heap::CELL => Shape::Ref((heap::field(v, 0), heap::field_desc(v, 0))),
                heap::COMPACT => Shape::Compact((heap::field(v, 0), heap::field_desc(v, 0))),
                heap::CLOSURE => Shape::Closure,
                heap::ONCE | heap::STACK => Shape::Continuation,
                heap::CHANNEL => Shape::Channel,
                heap::TASK => Shape::Task,
                heap::TVAR => Shape::TVar,
                k => Shape::Other(format!("<object of kind {k}>")),
            },
        }
    }

    fn role_of(&self, name: &str) -> Option<Role> {
        role_of(name)
    }
}
