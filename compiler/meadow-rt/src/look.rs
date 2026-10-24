//! **A value as text**, once for every runtime: what `show` renders, and what
//! `print` writes.
//!
//! A program can observe both -- `show` is a primitive -- and the differential
//! tests compare every engine's output with the CEK machine's character for
//! character, so the runtimes must render alike. Glade and Silo each kept a
//! renderer of their own; now each says only what shape one of its values
//! has ([`Look`]), and the rendering is here (`docs/RUNTIME-MERGE.md`,
//! phase 1).
//!
//! Two library types are taken apart by the roles their constructors play, as
//! the CEK machine takes them:
//!
//! * a **`List`** is a `Cons` chain, and prints as `[1; 2; 3]`;
//! * a **`Vector`** is a balanced tree, and prints as the sequence it is,
//!   `[1, 2, 3]`.
//!
//! Lists and vectors are flattened, so a long one prints whole; other nesting
//! past [`MAX_DEPTH`] prints as `…`, so that a deep enough value -- a long
//! chain of a user's data type, or a cycle through a `Ref` -- cannot overflow
//! the stack.

use crate::num::Width;
use crate::roles::{Role, TUPLE};
use num_bigint::BigInt;
use std::fmt::Write;

/// How deep printing goes into anything but a list or a vector.
pub const MAX_DEPTH: usize = 1000;

/// What a runtime's value is, as printing sees it: its parts are values of
/// the runtime's own again.
pub enum Shape<V> {
    Int(i64),
    Word(Width, u64),
    Float(f64),
    Float32(f32),
    Bool(bool),
    Unit,
    /// A character, or `None` for a word that is not one.
    Char(Option<char>),
    /// An interned name: a record's label, an effect operation's key.
    Sym(String),
    /// A string's text.
    Str(String),
    BigInt(BigInt),
    Array(Vec<V>),
    MutArray(Vec<V>),
    /// Each field by its label, in the record's order.
    Record(Vec<(String, V)>),
    Ref(V),
    Compact(V),
    Closure,
    Continuation,
    Channel,
    Task,
    TVar,
    /// Data: its constructor's canonical name -- `None` where the program
    /// does not name it -- its tag, and its fields.
    Data(Option<String>, u64, Vec<V>),
    /// Anything else, as it is to be shown.
    Other(String),
}

/// What a runtime says of its values for printing.
pub trait Look {
    type Value: Copy;

    fn shape(&self, v: Self::Value) -> Shape<Self::Value>;
    /// The role the constructor called `name` plays in this program, if any.
    fn role_of(&self, name: &str) -> Option<Role>;
}

/// A constructor as a reader wants to see it. The name a value carries is
/// canonical -- `Maybe.Just` -- because two types may each have a `Leaf`;
/// a person reading output has the context the qualifier supplies, so
/// printing it would be noise: `Just(3)`, not `Maybe.Just(3)`.
fn bare_ctor(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((_, c)) => c,
        None => name,
    }
}

/// `v` as `show` renders it, and as the REPL prints it.
pub fn show<L: Look>(l: &L, v: L::Value) -> String {
    let mut out = String::new();
    render(l, &mut out, v, 0);
    out
}

/// What `print` writes: a string as its text, an interned name as itself, and
/// anything else as [`show`] renders it.
pub fn displayed<L: Look>(l: &L, v: L::Value) -> String {
    match l.shape(v) {
        Shape::Str(text) | Shape::Sym(text) => text,
        _ => show(l, v),
    }
}

/// `v` flattened, if it is a `List` (a `Cons` chain) or a `Vector`.
fn elements<L: Look>(
    l: &L,
    name: &str,
    fields: Vec<L::Value>,
) -> Option<(Vec<L::Value>, &'static str)> {
    match l.role_of(name)? {
        Role::Nil | Role::Cons => list_items(l, name, fields).map(|xs| (xs, "; ")),
        Role::VectorEmpty | Role::VectorSingle | Role::VectorFull => {
            vector_elems(l, name, fields).map(|xs| (xs, ", "))
        }
        _ => None,
    }
}

fn list_items<L: Look>(l: &L, name: &str, fields: Vec<L::Value>) -> Option<Vec<L::Value>> {
    let mut out = Vec::new();
    let (mut role, mut fields) = (l.role_of(name)?, fields);
    loop {
        match (role, fields.len()) {
            (Role::Nil, 0) => return Some(out),
            (Role::Cons, 2) => {
                out.push(fields[0]);
                let Shape::Data(Some(next), _, f) = l.shape(fields[1]) else {
                    return None;
                };
                role = l.role_of(&next)?;
                fields = f;
            }
            _ => return None,
        }
    }
}

fn array_of<L: Look>(l: &L, v: L::Value) -> Option<Vec<L::Value>> {
    match l.shape(v) {
        Shape::Array(xs) => Some(xs),
        _ => None,
    }
}

/// A vector's elements, in order: `vBuildTree`'s layout in
/// `Std.Collections.Vector` -- a `Full`'s outer and inner front buffers, its
/// tree of nodes, then its inner and outer back buffers.
fn vector_elems<L: Look>(l: &L, name: &str, f: Vec<L::Value>) -> Option<Vec<L::Value>> {
    match (l.role_of(name)?, f.len()) {
        (Role::VectorEmpty, 0) => Some(Vec::new()),
        (Role::VectorSingle, 1) => array_of(l, f[0]),
        (Role::VectorFull, 7) => {
            let mut out = Vec::new();
            for i in [2usize, 3] {
                out.extend(array_of(l, f[i])?);
            }
            vector_node(l, f[4], &mut out)?;
            for i in [5usize, 6] {
                out.extend(array_of(l, f[i])?);
            }
            Some(out)
        }
        _ => None,
    }
}

fn vector_node<L: Look>(l: &L, v: L::Value, out: &mut Vec<L::Value>) -> Option<()> {
    let Shape::Data(Some(name), _, f) = l.shape(v) else {
        return None;
    };
    match (l.role_of(&name)?, f.len()) {
        (Role::VNodeLeaf, 1) => {
            out.extend(array_of(l, f[0])?);
            Some(())
        }
        (Role::VNodeBranch, 2) => {
            for kid in array_of(l, f[1])? {
                vector_node(l, kid, out)?;
            }
            Some(())
        }
        _ => None,
    }
}

fn join<L: Look>(l: &L, out: &mut String, vs: &[L::Value], sep: &str, depth: usize) {
    for (i, v) in vs.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        render(l, out, *v, depth + 1);
    }
}

fn render<L: Look>(l: &L, out: &mut String, v: L::Value, depth: usize) {
    if depth > MAX_DEPTH {
        out.push('…');
        return;
    }
    match l.shape(v) {
        Shape::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Shape::Word(w, b) => {
            let _ = write!(out, "{}", w.value(b));
        }
        Shape::Float(x) => out.push_str(&crate::fmt_float(x)),
        Shape::Float32(x) => out.push_str(&crate::num::fmt_float32(x)),
        // Printed as the constructors a program names them by.
        Shape::Bool(b) => out.push_str(if b { "True" } else { "False" }),
        Shape::Unit => out.push_str("()"),
        Shape::Char(Some(c)) => {
            let _ = write!(out, "{c:?}");
        }
        Shape::Char(None) => out.push('?'),
        Shape::Sym(s) | Shape::Str(s) => {
            let _ = write!(out, "{s:?}");
        }
        Shape::BigInt(n) => {
            let _ = write!(out, "{n}");
        }
        Shape::Array(xs) => {
            out.push_str("#[");
            join(l, out, &xs, ", ", depth);
            out.push(']');
        }
        Shape::MutArray(xs) => {
            out.push_str("mut #[");
            join(l, out, &xs, ", ", depth);
            out.push(']');
        }
        Shape::Record(fields) => {
            out.push_str("{ ");
            for (j, (label, x)) in fields.into_iter().enumerate() {
                if j > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{label} = ");
                render(l, out, x, depth + 1);
            }
            out.push_str(" }");
        }
        // The contents, not the address: what a person debugging one wants
        // to see.
        Shape::Ref(x) => {
            out.push_str("ref ");
            render(l, out, x, depth + 1);
        }
        Shape::Compact(x) => {
            out.push_str("compact ");
            render(l, out, x, depth + 1);
        }
        Shape::Closure => out.push_str("<closure>"),
        Shape::Continuation => out.push_str("<continuation>"),
        Shape::Channel => out.push_str("<channel>"),
        Shape::Task => out.push_str("<thread>"),
        Shape::TVar => out.push_str("<tvar>"),
        Shape::Other(s) => out.push_str(&s),
        Shape::Data(None, tag, _) => {
            let _ = write!(out, "#{tag}(..)");
        }
        Shape::Data(Some(name), _, fields) => render_data(l, out, &name, fields, depth),
    }
}

fn render_data<L: Look>(l: &L, out: &mut String, name: &str, fields: Vec<L::Value>, depth: usize) {
    if name == TUPLE {
        out.push('(');
        join(l, out, &fields, ", ", depth);
        out.push(')');
        return;
    }
    if matches!(
        l.role_of(name),
        Some(Role::Nil | Role::Cons | Role::VectorEmpty | Role::VectorSingle | Role::VectorFull)
    ) {
        match elements(l, name, fields) {
            Some((xs, sep)) => {
                out.push('[');
                join(l, out, &xs, sep, depth);
                out.push(']');
            }
            None => {
                let _ = write!(out, "{}(..)", bare_ctor(name));
            }
        }
        return;
    }
    if fields.is_empty() {
        out.push_str(bare_ctor(name));
        return;
    }
    let _ = write!(out, "{}(", bare_ctor(name));
    join(l, out, &fields, ", ", depth);
    out.push(')');
}
