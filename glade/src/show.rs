//! Printing and structural equality.
//!
//! Both have to agree with `meadow_eval` exactly. Printing, because `show` is a
//! primitive and a program can therefore observe it, and because the differential
//! tests compare rendered results. Equality, because `==` is structural at every
//! type and is one of the most used operations in the standard library.
//!
//! Two library types get special treatment, for the same reasons they do in the
//! CEK machine:
//!
//! * a **`List`** is a `Cons` chain, and prints as `[1; 2; 3]`;
//! * a **`Vector`** is a balanced tree, so two equal ones need not have equal
//!   shapes. Both printing and equality flatten it to the sequence it denotes.
//!
//! Everything here walks the heap iteratively where the structure can be deep,
//! which is the same reason the CEK's versions are iterative — a list is as deep
//! as it is long.

use crate::Error;
use crate::heap::Kind;
use crate::value::Value;
use crate::vm::Vm;
use meadow_rt::roles::{Role, TUPLE};

use num_bigint::{BigInt, Sign};
use std::fmt::Write;

/// A constructor as a reader wants to see it.
///
/// The name a value carries is canonical -- `Maybe.Just` -- because two types
/// may each have a `Leaf` and something has to tell them apart. A person
/// reading output has the context the qualifier supplies, so printing it would
/// be noise: `Just(3)`, not `Maybe.Just(3)`.
fn bare_ctor(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((_, c)) => c,
        None => name,
    }
}

impl Vm<'_> {
    /// The constructor playing `role` in this program, if it declares one --
    /// see `meadow_rt::roles`.
    pub(crate) fn role(&self, role: Role) -> Option<meadow_intern::InternedString> {
        self.program
            .roles
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, c)| *c)
    }

    /// The tag of the constructor playing `role`: one no `switch` arm has
    /// where the program declares none, as [`Vm::ctor_tag`] gives an unknown
    /// name.
    pub(crate) fn role_tag(&self, role: Role) -> u32 {
        self.role(role).map_or(u32::MAX, |c| self.ctor_tag(&c))
    }

    /// The role the constructor called `name` plays, if any.
    pub(crate) fn role_of(&self, name: &str) -> Option<Role> {
        self.program
            .roles
            .iter()
            .find(|(_, c)| &**c == name)
            .map(|(r, _)| *r)
    }

    /// Is `name` one of a vector's constructors?
    pub(crate) fn is_vector_ctor(&self, name: &str) -> bool {
        matches!(
            self.role_of(name),
            Some(Role::VectorEmpty | Role::VectorSingle | Role::VectorFull)
        )
    }

    /// The tag a constructor has in this program, for the primitives that build
    /// one. A name the program never mentions gets a tag no `switch` arm has,
    /// which is the same thing the AxCut machine does.
    pub(crate) fn ctor_tag(&self, name: &str) -> u32 {
        self.program
            .ctors
            .iter()
            .position(|c| &**c == name)
            .map(|i| i as u32)
            .unwrap_or(u32::MAX)
    }

    /// A `Cons` chain as its elements, or `None` if it is not one.
    pub(crate) fn list_items(&self, v: Value) -> Option<Vec<Value>> {
        let mut out = Vec::new();
        let mut cur = v;
        loop {
            let a = cur.addr()?;
            if self.heap.kind(a) != Kind::Data {
                return None;
            }
            let name = self.program.ctor(self.heap.meta(a))?;
            match (self.role_of(&name), self.heap.len(a)) {
                (Some(Role::Nil), 0) => return Some(out),
                (Some(Role::Cons), 2) => {
                    out.push(self.heap.field(a, 0));
                    cur = self.heap.field(a, 1);
                }
                _ => return None,
            }
        }
    }

    /// A `Vector` flattened to its elements, in order.
    pub(crate) fn vector_elems(&self, v: Value) -> Option<Vec<Value>> {
        let a = v.addr()?;
        if self.heap.kind(a) != Kind::Data {
            return None;
        }
        let name = self.program.ctor(self.heap.meta(a))?;
        let n = self.heap.len(a);
        match (self.role_of(&name), n) {
            (Some(Role::VectorEmpty), 0) => Some(Vec::new()),
            (Some(Role::VectorSingle), 1) => self.array_elems(self.heap.field(a, 0)),
            (Some(Role::VectorFull), 7) => {
                let mut out = Vec::new();
                for i in [2usize, 3] {
                    out.extend(self.array_elems(self.heap.field(a, i))?);
                }
                self.vector_node(self.heap.field(a, 4), &mut out)?;
                for i in [5usize, 6] {
                    out.extend(self.array_elems(self.heap.field(a, i))?);
                }
                Some(out)
            }
            _ => None,
        }
    }

    fn vector_node(&self, v: Value, out: &mut Vec<Value>) -> Option<()> {
        let a = v.addr()?;
        if self.heap.kind(a) != Kind::Data {
            return None;
        }
        let name = self.program.ctor(self.heap.meta(a))?;
        match (self.role_of(&name), self.heap.len(a)) {
            (Some(Role::VNodeLeaf), 1) => {
                out.extend(self.array_elems(self.heap.field(a, 0))?);
                Some(())
            }
            (Some(Role::VNodeBranch), 2) => {
                for kid in self.array_elems(self.heap.field(a, 1))? {
                    self.vector_node(kid, out)?;
                }
                Some(())
            }
            _ => None,
        }
    }

    pub(crate) fn array_elems(&self, v: Value) -> Option<Vec<Value>> {
        let a = v.addr()?;
        if !self.heap.kind(a).is_array() {
            return None;
        }
        Some(self.heap.array_values(a))
    }

    pub(crate) fn bigint_at(&self, v: Value) -> Option<BigInt> {
        let a = v.addr()?;
        if self.heap.kind(a) != Kind::BigInt {
            return None;
        }
        let sign = match self.heap.meta(a) {
            crate::prims::big::ZERO => Sign::NoSign,
            crate::prims::big::PLUS => Sign::Plus,
            _ => Sign::Minus,
        };
        // 64-bit limbs, least significant first. num-bigint builds from `u32`
        // digits or from bytes, and bytes are the one of the two that takes
        // the limbs as they are rather than splitting each in half.
        let mut bytes = Vec::with_capacity(self.heap.len(a) * 8);
        for limb in self.heap.words(a) {
            bytes.extend_from_slice(&limb.to_le_bytes());
        }
        Some(BigInt::from_bytes_le(sign, &bytes))
    }

    /// How `print` and `println` render a value: a `String` as its text,
    /// everything else as [`Vm::show`] gives it.
    ///
    /// `println "hello"` used to write `"hello"`, quotes and all, because these
    /// are polymorphic and fell through to the display used for data.
    pub fn displayed(&self, v: Value) -> String {
        match self.text_of(v) {
            Some(text) => text,
            None => self.show(v),
        }
    }

    /// Render a value the way the REPL prints it.
    pub fn show(&self, v: Value) -> String {
        let mut out = String::new();
        self.render(&mut out, v, 0);
        out
    }

    fn render(&self, out: &mut String, v: Value, depth: usize) {
        // `render` recurses once per level of nesting, so a value deep enough
        // -- a long chain of a user data type, or a cycle through a `Ref` --
        // would overflow the native stack and abort the process. Past a bound
        // it prints an ellipsis instead. Lists and vectors are flattened
        // below and do not count against it, so an ordinary long list still
        // prints in full.
        const MAX_DEPTH: usize = 1000;
        if depth > MAX_DEPTH {
            out.push('…');
            return;
        }
        match v {
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(x) => out.push_str(&meadow_rt::fmt_float(x)),
            Value::Word(w, b) => {
                let _ = write!(out, "{}", w.value(b));
            }
            Value::Float32(x) => out.push_str(&meadow_rt::num::fmt_float32(x)),
            // Printed as the constructors a program names them by.
            Value::Bool(b) => out.push_str(if b { "True" } else { "False" }),
            Value::Str(s) => {
                let _ = write!(out, "{:?}", &*s);
            }
            Value::Char(c) => {
                let _ = write!(out, "{c:?}");
            }
            Value::Unit => out.push_str("()"),
            Value::Obj(a) => match self.heap.kind(a) {
                Kind::BigInt => {
                    let _ = write!(out, "{}", self.bigint_at(v).expect("a bigint"));
                }
                Kind::Array | Kind::Bytes => {
                    out.push_str("#[");
                    self.join(out, &self.heap.array_values(a), ", ", depth);
                    out.push(']');
                }
                Kind::MutArray => {
                    out.push_str("mut #[");
                    self.join(out, &self.heap.fields(a), ", ", depth);
                    out.push(']');
                }
                Kind::Record => {
                    out.push_str("{ ");
                    for j in 0..self.heap.len(a) / 2 {
                        if j > 0 {
                            out.push_str(", ");
                        }
                        if let Value::Str(l) = self.heap.field(a, 2 * j) {
                            let _ = write!(out, "{l} = ");
                        }
                        self.render(out, self.heap.field(a, 2 * j + 1), depth + 1);
                    }
                    out.push_str(" }");
                }
                // The contents, not the address: what a person debugging one
                // wants to see.
                Kind::Ref => {
                    out.push_str("ref ");
                    self.render(out, self.heap.field(a, 0), depth + 1);
                }
                Kind::Closure => out.push_str("<closure>"),
                Kind::Resume | Kind::Frame | Kind::Stack => out.push_str("<continuation>"),
                Kind::Compact => {
                    out.push_str("compact ");
                    self.render(out, self.heap.field(a, 0), depth + 1);
                }
                Kind::Channel => out.push_str("<channel>"),
                Kind::Task => out.push_str("<thread>"),
                Kind::TVar => out.push_str("<tvar>"),
                Kind::Str => {
                    let text = String::from_utf8_lossy(&self.heap.packed_bytes(a)).into_owned();
                    let _ = write!(out, "{text:?}");
                }
                Kind::Data => self.render_data(out, v, a, depth),
            },
        }
    }

    fn render_data(&self, out: &mut String, v: Value, a: crate::value::Addr, depth: usize) {
        let name = match self.program.ctor(self.heap.meta(a)) {
            Some(n) => n,
            None => {
                let _ = write!(out, "#{}(..)", self.heap.meta(a));
                return;
            }
        };
        match &*name {
            TUPLE => {
                out.push('(');
                self.join(out, &self.heap.fields(a), ", ", depth);
                out.push(')');
            }
            n if matches!(self.role_of(n), Some(Role::Nil | Role::Cons)) => {
                match self.list_items(v) {
                    Some(xs) => {
                        out.push('[');
                        self.join(out, &xs, "; ", depth);
                        out.push(']');
                    }
                    None => {
                        let _ = write!(out, "{}(..)", bare_ctor(&name));
                    }
                }
            }
            n if self.is_vector_ctor(n) => match self.vector_elems(v) {
                Some(xs) => {
                    out.push('[');
                    self.join(out, &xs, ", ", depth);
                    out.push(']');
                }
                None => {
                    let _ = write!(out, "{}(..)", bare_ctor(&name));
                }
            },
            _ if self.heap.len(a) == 0 => {
                let _ = write!(out, "{}", bare_ctor(&name));
            }
            _ => {
                let _ = write!(out, "{}(", bare_ctor(&name));
                self.join(out, &self.heap.fields(a), ", ", depth);
                out.push(')');
            }
        }
    }

    fn join(&self, out: &mut String, vs: &[Value], sep: &str, depth: usize) {
        for (i, v) in vs.iter().enumerate() {
            if i > 0 {
                out.push_str(sep);
            }
            self.render(out, *v, depth + 1);
        }
    }

    /// Structural equality.
    ///
    /// Iterative, because a `Cons` chain is as deep as it is long. The address
    /// check at the top is what makes `xs == xs` on a long list O(1) rather than
    /// O(n) — the same short-circuit the CEK gets from `Rc::ptr_eq`, and here it
    /// is simply integer equality.
    /// Hash a `Kind::Str` from the heap without copying its bytes out.
    ///
    /// A `Kind::Str` holds its bytes eight to a little-endian word with the
    /// last word's spare bytes zero, which is the very layout
    /// [`meadow_core::hash::Hasher::str`] hashes -- so the words go straight in
    /// and the result is the same as if they had been made a `String` first.
    fn hash_packed(&self, a: crate::value::Addr, h: &mut meadow_rt::hash::Hasher) {
        let len = self.heap.meta(a) as usize;
        h.str_packed(
            len,
            (0..len.div_ceil(8)).map(|i| self.heap.field_word(a, i)),
        );
    }

    /// Hash `v` into `h` if it has no parts, answering whether it did.
    ///
    /// `false` means the value is a data value, an array, a record or a compact
    /// -- something with children, which [`Vm::hash_value`] walks with a work
    /// stack. An `Err` is a value that cannot be hashed at all, which is the
    /// same answer either way and is better given without allocating first.
    fn hash_flat(&self, v: Value, h: &mut meadow_rt::hash::Hasher) -> Result<bool, Error> {
        use meadow_rt::hash::unhashable;
        match v {
            Value::Int(_) | Value::Word(..) | Value::Float(_) | Value::Float32(_) => {
                meadow_rt::num::hash_into(h, &self.num(v)?)
            }
            Value::Bool(b) => h.bool(b),
            Value::Char(c) => h.char(c),
            Value::Str(s) => h.str(&s),
            Value::Unit => h.unit(),
            Value::Obj(a) => {
                return match self.heap.kind(a) {
                    Kind::Str => {
                        self.hash_packed(a, h);
                        Ok(true)
                    }
                    Kind::BigInt => match self.bigint_at(v) {
                        Some(b) => {
                            meadow_rt::num::hash_into(h, &meadow_rt::num::Num::Big(b));
                            Ok(true)
                        }
                        None => Err(Error {
                            msg: "hash: a malformed BigInt".into(),
                        }),
                    },
                    Kind::Ref => Err(Error {
                        msg: unhashable("a Ref"),
                    }),
                    Kind::MutArray => Err(Error {
                        msg: unhashable("a mutable array"),
                    }),
                    Kind::Closure | Kind::Resume | Kind::Frame | Kind::Stack => Err(Error {
                        msg: unhashable("a function"),
                    }),
                    Kind::Channel => Err(Error {
                        msg: unhashable("a channel"),
                    }),
                    Kind::Task => Err(Error {
                        msg: unhashable("a thread"),
                    }),
                    Kind::TVar => Err(Error {
                        msg: unhashable("a TVar"),
                    }),
                    // Has parts: the caller walks it.
                    _ => Ok(false),
                };
            }
        }
        Ok(true)
    }

    /// `hash`, fed to [`meadow_core::hash::Hasher`] in the order every engine
    /// uses: a value's head, then its parts left to right.
    pub fn hash_value(&self, v: Value) -> Result<i64, Error> {
        use meadow_rt::hash::{Hasher, unhashable};
        enum Work {
            Val(Value),
            Label(String),
        }
        let mut h = Hasher::new();
        // A value with no parts needs no work stack, and the work stack is a
        // `Vec` -- a call to the allocator on the way in and another on the way
        // out. The keys people actually hash are all here: a string, an
        // integer, a character. `wordfreq` hashes a string twice per word, so
        // this was two million allocations it did not need.
        if self.hash_flat(v, &mut h)? {
            return Ok(h.finish());
        }
        let mut stack = vec![Work::Val(v)];
        while let Some(w) = stack.pop() {
            let v = match w {
                Work::Label(l) => {
                    h.str(&l);
                    continue;
                }
                Work::Val(v) => v,
            };
            let a = match v {
                Value::Int(_) | Value::Word(..) | Value::Float(_) | Value::Float32(_) => {
                    meadow_rt::num::hash_into(&mut h, &self.num(v)?);
                    continue;
                }
                Value::Bool(b) => {
                    h.bool(b);
                    continue;
                }
                Value::Char(c) => {
                    h.char(c);
                    continue;
                }
                Value::Str(s) => {
                    h.str(&s);
                    continue;
                }
                Value::Unit => {
                    h.unit();
                    continue;
                }
                Value::Obj(a) => a,
            };
            let n = self.heap.len(a);
            match self.heap.kind(a) {
                Kind::Bytes => {
                    let n = self.heap.array_len(a);
                    h.array(n);
                    stack.extend((0..n).rev().map(|i| Work::Val(self.heap.array_value(a, i))));
                }
                Kind::Data => {
                    let name = self.program.ctor(self.heap.meta(a));
                    let name = name.as_deref().unwrap_or("?");
                    if self.is_vector_ctor(name) {
                        let Some(xs) = self.vector_elems(v) else {
                            let msg = format!("hash: a malformed vector ({name})");
                            return Err(Error { msg });
                        };
                        h.vector(xs.len());
                        stack.extend(xs.into_iter().rev().map(Work::Val));
                    } else {
                        h.data(name, n);
                        stack.extend((0..n).rev().map(|i| Work::Val(self.heap.field(a, i))));
                    }
                }
                Kind::Array => {
                    h.array(n);
                    stack.extend((0..n).rev().map(|i| Work::Val(self.heap.field(a, i))));
                }
                Kind::Record => {
                    h.record(n / 2);
                    let mut sorted: Vec<(String, Value)> = (0..n / 2)
                        .map(|j| {
                            let label = match self.heap.field(a, 2 * j) {
                                Value::Str(l) => l.to_string(),
                                _ => String::new(),
                            };
                            (label, self.heap.field(a, 2 * j + 1))
                        })
                        .collect();
                    sorted.sort_by(|x, y| x.0.cmp(&y.0));
                    for (label, value) in sorted.into_iter().rev() {
                        stack.push(Work::Val(value));
                        stack.push(Work::Label(label));
                    }
                }
                Kind::BigInt => match self.bigint_at(v) {
                    Some(b) => meadow_rt::num::hash_into(&mut h, &meadow_rt::num::Num::Big(b)),
                    None => {
                        return Err(Error {
                            msg: "hash: a malformed BigInt".into(),
                        });
                    }
                },
                Kind::Ref => {
                    return Err(Error {
                        msg: unhashable("a Ref"),
                    });
                }
                Kind::MutArray => {
                    return Err(Error {
                        msg: unhashable("a mutable array"),
                    });
                }
                Kind::Closure | Kind::Resume | Kind::Frame | Kind::Stack => {
                    return Err(Error {
                        msg: unhashable("a function"),
                    });
                }
                Kind::Compact => {
                    h.compact();
                    stack.push(Work::Val(self.heap.field(a, 0)));
                }
                Kind::Channel => {
                    return Err(Error {
                        msg: unhashable("a channel"),
                    });
                }
                Kind::Task => {
                    return Err(Error {
                        msg: unhashable("a thread"),
                    });
                }
                Kind::TVar => {
                    return Err(Error {
                        msg: unhashable("a TVar"),
                    });
                }
                Kind::Str => self.hash_packed(a, &mut h),
            }
        }
        Ok(h.finish())
    }

    pub fn value_eq(&self, a: Value, b: Value) -> bool {
        let mut stack = vec![(a, b)];
        while let Some((a, b)) = stack.pop() {
            match (a, b) {
                (Value::Int(x), Value::Int(y)) if x == y => {}
                (Value::Float(x), Value::Float(y)) if x == y => {}
                // Numbers by value, so a literal in generic code equals the
                // value it stands beside -- see `meadow_core::num`.
                (x, y) if is_number(x) || is_number(y) => {
                    match (self.try_num(x), self.try_num(y)) {
                        (Some(p), Some(q)) if meadow_rt::num::num_eq(&p, &q) => {}
                        _ => return false,
                    }
                }
                (Value::Bool(x), Value::Bool(y)) if x == y => {}
                (Value::Char(x), Value::Char(y)) if x == y => {}
                (Value::Str(x), Value::Str(y)) if x == y => {}
                (Value::Unit, Value::Unit) => {}
                (Value::Obj(x), Value::Obj(y)) => {
                    if x == y {
                        continue;
                    }
                    let (kx, ky) = (self.heap.kind(x), self.heap.kind(y));
                    match (kx, ky) {
                        (Kind::Data, Kind::Data) => {
                            let nx = self.program.ctor(self.heap.meta(x));
                            let ny = self.program.ctor(self.heap.meta(y));
                            // A `Vector` is a tree, so compare the sequences the
                            // two denote rather than their shapes.
                            let vectors = matches!((nx, ny), (Some(nx), Some(ny))
                                if self.is_vector_ctor(&nx) && self.is_vector_ctor(&ny));
                            if vectors {
                                match (self.vector_elems(a), self.vector_elems(b)) {
                                    (Some(xs), Some(ys)) if xs.len() == ys.len() => {
                                        stack.extend(xs.into_iter().zip(ys));
                                        continue;
                                    }
                                    _ => return false,
                                }
                            }
                            if self.heap.meta(x) != self.heap.meta(y)
                                || self.heap.len(x) != self.heap.len(y)
                            {
                                return false;
                            }
                            for i in 0..self.heap.len(x) {
                                stack.push((self.heap.field(x, i), self.heap.field(y, i)));
                            }
                        }
                        (Kind::Bytes, Kind::Bytes) => {
                            if !self.heap.packed_eq(x, y) {
                                return false;
                            }
                        }
                        (kx, ky) if kx.is_array() && ky.is_array() => {
                            let n = self.heap.array_len(x);
                            if n != self.heap.array_len(y) {
                                return false;
                            }
                            for i in 0..n {
                                stack.push((
                                    self.heap.array_value(x, i),
                                    self.heap.array_value(y, i),
                                ));
                            }
                        }
                        (Kind::Record, Kind::Record) => {
                            if self.heap.len(x) != self.heap.len(y) {
                                return false;
                            }
                            for j in 0..self.heap.len(x) / 2 {
                                let Value::Str(label) = self.heap.field(x, 2 * j) else {
                                    return false;
                                };
                                match self.record_get(y, label) {
                                    Some(w) => stack.push((self.heap.field(x, 2 * j + 1), w)),
                                    None => return false,
                                }
                            }
                        }
                        (Kind::Str, Kind::Str) => {
                            if !self.heap.packed_eq(x, y) {
                                return false;
                            }
                        }
                        // One representation per number -- normalized limbs, the
                        // sign apart -- so equal numbers have equal words.
                        (Kind::BigInt, Kind::BigInt) => {
                            if self.heap.meta(x) != self.heap.meta(y)
                                || !self.heap.words(x).eq(self.heap.words(y))
                            {
                                return false;
                            }
                        }
                        // Immutable, so two are equal when what they hold is.
                        (Kind::Compact, Kind::Compact) => {
                            stack.push((self.heap.field(x, 0), self.heap.field(y, 0)));
                        }
                        // Handles copied between heaps are different objects
                        // naming the same channel or thread.
                        (Kind::Channel, Kind::Channel)
                        | (Kind::Task, Kind::Task)
                        | (Kind::TVar, Kind::TVar)
                            if self.heap.meta(x) == self.heap.meta(y) => {}
                        // A `Ref` is a place, and two cells holding the same
                        // thing are still two cells. The address check above is
                        // the only way one compares equal.
                        _ => return false,
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

/// An immediate number -- the cheap test, before a `BigInt` is ever read.
fn is_number(v: Value) -> bool {
    matches!(
        v,
        Value::Int(_) | Value::Word(..) | Value::Float(_) | Value::Float32(_)
    )
}
