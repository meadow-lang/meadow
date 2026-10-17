//! A value as text, exactly as `meadow-glade` renders it -- the differential
//! tests compare the two backends' output character for character -- and the
//! program's tables it reads names out of.

use crate::heap::{self, Word};
use meadow_rt::desc;
use meadow_rt::roles::{Role, TUPLE};
use std::fmt::Write;

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

/// The emitted module's tables, read once.
pub fn names() -> &'static Names {
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
    let n = names();
    match n.syms.get(i) {
        Some(s) => s.clone(),
        None => EXTRA
            .lock()
            .map(|e| e.get(i - n.syms.len()).cloned().unwrap_or_default())
            .unwrap_or_default(),
    }
}

/// The symbol called `name`.
pub fn sym_named(name: &str) -> usize {
    let n = names();
    if let Some(i) = n.syms.iter().position(|s| s == name) {
        return i;
    }
    let mut extra = EXTRA.lock().unwrap_or_else(|p| p.into_inner());
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

fn bare_ctor(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((_, c)) => c,
        None => name,
    }
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
    if v & 1 == 1 {
        return Some(ctor_name((v >> 1) as usize));
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
    let tag = if v & 1 == 1 {
        (v >> 1) as usize
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

fn array_elems(v: Word) -> Option<Vec<(Word, i64)>> {
    (heap::is_block(v) && heap::kind(v) == heap::ARRAY).then(|| fields_of(v))
}

fn list_items(mut v: Word) -> Option<Vec<(Word, i64)>> {
    let mut out = Vec::new();
    loop {
        match role_of(&ctor_of(v)?) {
            Some(Role::Nil) => return Some(out),
            Some(Role::Cons) => {
                let f = fields_of(v);
                if f.len() != 2 {
                    return None;
                }
                out.push(f[0]);
                v = f[1].0;
            }
            _ => return None,
        }
    }
}

/// `v`, described by `d`, as `show` renders it.
pub fn show(v: Word, d: i64) -> String {
    let mut out = String::new();
    render(&mut out, v, d);
    out
}

/// What `print` writes: a string as itself, anything else as shown.
pub fn displayed(v: Word, d: i64) -> String {
    if d == desc::REF && heap::is_block(v) && heap::kind(v) == heap::STRING {
        return String::from_utf8_lossy(&heap::bytes(v)).into_owned();
    }
    if d == desc::STR {
        return sym_name(v as usize);
    }
    show(v, d)
}

fn render(out: &mut String, v: Word, d: i64) {
    match d {
        desc::INT => {
            let _ = write!(out, "{}", v as i64);
        }
        desc::FLOAT => out.push_str(&meadow_rt::fmt_float(f64::from_bits(v))),
        desc::FLOAT32 => out.push_str(&meadow_rt::num::fmt_float32(f32::from_bits(v as u32))),
        desc::BOOL => out.push_str(if v != 0 { "True" } else { "False" }),
        desc::UNIT => out.push_str("()"),
        desc::CHAR => match char::from_u32(v as u32) {
            Some(c) => {
                let _ = write!(out, "{c:?}");
            }
            None => out.push('?'),
        },
        desc::STR => {
            let _ = write!(out, "{:?}", sym_name(v as usize));
        }
        d if (desc::WORD..desc::WORD + 7).contains(&d) => {
            let w = meadow_rt::num::Width::ALL[(d - desc::WORD) as usize];
            let _ = write!(out, "{}", w.value(v));
        }
        _ => render_ref(out, v),
    }
}

fn join(out: &mut String, items: &[(Word, i64)], sep: &str) {
    for (i, (x, d)) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        render(out, *x, *d);
    }
}

fn render_ref(out: &mut String, v: Word) {
    if v == 0 {
        out.push_str("<continuation>");
        return;
    }
    if v & 1 == 1 || heap::kind(v) == heap::DATA {
        render_data(out, v);
        return;
    }
    match heap::kind(v) {
        heap::STRING => {
            let text = String::from_utf8_lossy(&heap::bytes(v)).into_owned();
            let _ = write!(out, "{text:?}");
        }
        heap::BIGINT => {
            let _ = write!(
                out,
                "{}",
                crate::value::bigint(v).expect("a BigInt's block")
            );
        }
        heap::ARRAY => {
            out.push_str("#[");
            join(out, &fields_of(v), ", ");
            out.push(']');
        }
        heap::MUT_ARRAY => {
            out.push_str("mut #[");
            join(out, &fields_of(v), ", ");
            out.push(']');
        }
        heap::RECORD => {
            out.push_str("{ ");
            let f = fields_of(v);
            for j in 0..f.len() / 2 {
                if j > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{} = ", sym_name(f[2 * j].0 as usize));
                render(out, f[2 * j + 1].0, f[2 * j + 1].1);
            }
            out.push_str(" }");
        }
        heap::CELL => {
            out.push_str("ref ");
            render(out, heap::field(v, 0), heap::field_desc(v, 0));
        }
        heap::COMPACT => {
            out.push_str("compact ");
            render(out, heap::field(v, 0), heap::field_desc(v, 0));
        }
        heap::CLOSURE => out.push_str("<closure>"),
        heap::ONCE | heap::STACK => out.push_str("<continuation>"),
        heap::CHANNEL => out.push_str("<channel>"),
        heap::TASK => out.push_str("<thread>"),
        heap::TVAR => out.push_str("<tvar>"),
        k => {
            let _ = write!(out, "<object of kind {k}>");
        }
    }
}

fn render_data(out: &mut String, v: Word) {
    let name = ctor_of(v).unwrap_or_default();
    let f = fields_of(v);
    match name.as_str() {
        "" => {
            let _ = write!(out, "#{}(..)", if v & 1 == 1 { v >> 1 } else { 0 });
        }
        TUPLE => {
            out.push('(');
            join(out, &f, ", ");
            out.push(')');
        }
        n if matches!(role_of(n), Some(Role::Nil | Role::Cons)) => match list_items(v) {
            Some(xs) => {
                out.push('[');
                join(out, &xs, "; ");
                out.push(']');
            }
            None => {
                let _ = write!(out, "{}(..)", bare_ctor(&name));
            }
        },
        n if is_vector_ctor(n) => match vector_elems(v) {
            Some(xs) => {
                out.push('[');
                join(out, &xs, ", ");
                out.push(']');
            }
            None => {
                let _ = write!(out, "{}(..)", bare_ctor(&name));
            }
        },
        _ if f.is_empty() => out.push_str(bare_ctor(&name)),
        _ => {
            let _ = write!(out, "{}(", bare_ctor(&name));
            join(out, &f, ", ");
            out.push(')');
        }
    }
}
